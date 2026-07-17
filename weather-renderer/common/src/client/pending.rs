use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::oneshot;
use weather_schema::RpcResponse;

use super::failure::ClientFailure;

type PendingReply = Result<RpcResponse, ClientFailure>;

#[derive(Clone)]
pub(super) struct PendingRegistry {
    state: Arc<Mutex<PendingState>>,
}

#[derive(Default)]
struct PendingState {
    entries: HashMap<String, PendingEntry>,
    terminal: Option<ClientFailure>,
}

struct PendingEntry {
    marker: Arc<()>,
    sender: oneshot::Sender<PendingReply>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RegisterError {
    Collision,
    Terminal(ClientFailure),
}

pub(super) struct PendingLease {
    registry: PendingRegistry,
    request_id: String,
    marker: Arc<()>,
    receiver: oneshot::Receiver<PendingReply>,
}

impl PendingRegistry {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(PendingState::default())),
        }
    }

    pub(super) fn register(&self, request_id: String) -> Result<PendingLease, RegisterError> {
        let mut state = self.lock();
        if let Some(failure) = state.terminal.clone() {
            return Err(RegisterError::Terminal(failure));
        }

        let marker = Arc::new(());
        let (sender, receiver) = oneshot::channel();
        match state.entries.entry(request_id.clone()) {
            Entry::Occupied(_) => Err(RegisterError::Collision),
            Entry::Vacant(entry) => {
                entry.insert(PendingEntry {
                    marker: marker.clone(),
                    sender,
                });
                drop(state);
                Ok(PendingLease {
                    registry: self.clone(),
                    request_id,
                    marker,
                    receiver,
                })
            }
        }
    }

    pub(super) fn complete(&self, response: RpcResponse) -> bool {
        let entry = self.lock().entries.remove(&response.request_id);
        let Some(entry) = entry else {
            return false;
        };
        entry.sender.send(Ok(response)).is_ok()
    }

    pub(super) fn fail_all(&self, failure: ClientFailure) -> bool {
        let entries = {
            let mut state = self.lock();
            if state.terminal.is_some() {
                return false;
            }
            state.terminal = Some(failure.clone());
            std::mem::take(&mut state.entries)
        };
        for entry in entries.into_values() {
            let _ = entry.sender.send(Err(failure.clone()));
        }
        true
    }

    pub(super) fn terminal_failure(&self) -> Option<ClientFailure> {
        self.lock().terminal.clone()
    }

    fn remove_if_matches(&self, request_id: &str, marker: &Arc<()>) {
        let entry = {
            let mut state = self.lock();
            let matches = state
                .entries
                .get(request_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.marker, marker));
            matches.then(|| state.entries.remove(request_id)).flatten()
        };
        drop(entry);
    }

    fn lock(&self) -> MutexGuard<'_, PendingState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

impl PendingLease {
    pub(super) async fn receive(&mut self) -> PendingReply {
        match (&mut self.receiver).await {
            Ok(reply) => reply,
            Err(_) => Err(self.registry.terminal_failure().unwrap_or_else(|| {
                ClientFailure::background_task("pending response sender was dropped")
            })),
        }
    }
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        self.registry
            .remove_if_matches(&self.request_id, &self.marker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_collision_keeps_original_waiter() {
        let pending = PendingRegistry::new();
        let mut first = pending.register("same-request".to_string()).unwrap();

        assert!(matches!(
            pending.register("same-request".to_string()),
            Err(RegisterError::Collision)
        ));
        assert_eq!(pending.len(), 1);

        assert!(pending.complete(RpcResponse {
            request_id: "same-request".to_string(),
            ..Default::default()
        }));
        assert_eq!(first.receive().await.unwrap().request_id, "same-request");
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn dropping_pending_lease_removes_only_its_registration() {
        let pending = PendingRegistry::new();
        let lease = pending.register("request".to_string()).unwrap();
        assert_eq!(pending.len(), 1);

        drop(lease);

        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn first_terminal_failure_is_sticky_and_fails_all_waiters() {
        let pending = PendingRegistry::new();
        let mut first = pending.register("first".to_string()).unwrap();
        let mut second = pending.register("second".to_string()).unwrap();
        let failure = ClientFailure::rpc_receive("connection lost");

        assert!(pending.fail_all(failure.clone()));
        assert!(!pending.fail_all(ClientFailure::Closed));
        assert_eq!(first.receive().await.unwrap_err(), failure);
        assert_eq!(second.receive().await.unwrap_err(), failure);
        assert_eq!(pending.len(), 0);
        assert_eq!(pending.terminal_failure(), Some(failure.clone()));
        assert!(matches!(
            pending.register("third".to_string()),
            Err(RegisterError::Terminal(observed)) if observed == failure
        ));
    }
}
