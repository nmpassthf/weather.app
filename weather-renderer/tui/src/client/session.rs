use std::{fmt, future::Future};

use tokio::{
    sync::watch,
    task::{JoinHandle, JoinSet},
};

use super::{failure::ClientFailure, pending::PendingRegistry};

pub(super) type SessionTaskResult = Result<(), ClientFailure>;

#[derive(Clone, Copy, Debug)]
enum SessionTaskKind {
    RpcSend,
    RpcReceive,
    EventReceive,
}

impl fmt::Display for SessionTaskKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RpcSend => formatter.write_str("RPC send task"),
            Self::RpcReceive => formatter.write_str("RPC receive task"),
            Self::EventReceive => formatter.write_str("event receive task"),
        }
    }
}

pub(super) struct ClientSession {
    pending: PendingRegistry,
    shutdown: watch::Sender<bool>,
    done: watch::Sender<bool>,
    _supervisor: JoinHandle<()>,
}

impl ClientSession {
    pub(super) fn spawn<SendTask, ReceiveTask, EventTask>(
        pending: PendingRegistry,
        send_task: SendTask,
        receive_task: ReceiveTask,
        event_task: EventTask,
    ) -> Self
    where
        SendTask: Future<Output = SessionTaskResult> + Send + 'static,
        ReceiveTask: Future<Output = SessionTaskResult> + Send + 'static,
        EventTask: Future<Output = SessionTaskResult> + Send + 'static,
    {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (done, _) = watch::channel(false);
        let supervisor = tokio::spawn(supervise_session(
            pending.clone(),
            shutdown_rx,
            done.clone(),
            send_task,
            receive_task,
            event_task,
        ));
        Self {
            pending,
            shutdown,
            done,
            _supervisor: supervisor,
        }
    }

    pub(super) fn request_close(&self) {
        self.pending.fail_all(ClientFailure::Closed);
        self.shutdown.send_replace(true);
    }

    pub(super) async fn close(&self) {
        self.request_close();
        let mut done = self.done.subscribe();
        loop {
            if *done.borrow_and_update() {
                return;
            }
            if done.changed().await.is_err() {
                return;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn completion(&self) -> watch::Receiver<bool> {
        self.done.subscribe()
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.request_close();
    }
}

async fn supervise_session<SendTask, ReceiveTask, EventTask>(
    pending: PendingRegistry,
    mut shutdown: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    send_task: SendTask,
    receive_task: ReceiveTask,
    event_task: EventTask,
) where
    SendTask: Future<Output = SessionTaskResult> + Send + 'static,
    ReceiveTask: Future<Output = SessionTaskResult> + Send + 'static,
    EventTask: Future<Output = SessionTaskResult> + Send + 'static,
{
    let _completion = SessionCompletion {
        pending: pending.clone(),
        done,
    };
    let mut tasks = JoinSet::new();
    tasks.spawn(async move { (SessionTaskKind::RpcSend, send_task.await) });
    tasks.spawn(async move { (SessionTaskKind::RpcReceive, receive_task.await) });
    tasks.spawn(async move { (SessionTaskKind::EventReceive, event_task.await) });

    let failure = tokio::select! {
        _ = wait_for_shutdown(&mut shutdown) => ClientFailure::Closed,
        joined = tasks.join_next() => joined_failure(joined),
    };
    pending.fail_all(failure);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn joined_failure(
    joined: Option<Result<(SessionTaskKind, SessionTaskResult), tokio::task::JoinError>>,
) -> ClientFailure {
    match joined {
        Some(Ok((_, Err(failure)))) => failure,
        Some(Ok((kind, Ok(())))) => {
            ClientFailure::background_task(format_args!("{kind} exited unexpectedly"))
        }
        Some(Err(error)) => ClientFailure::background_task(error),
        None => ClientFailure::background_task("all client tasks exited unexpectedly"),
    }
}

struct SessionCompletion {
    pending: PendingRegistry,
    done: watch::Sender<bool>,
}

impl Drop for SessionCompletion {
    fn drop(&mut self) {
        self.pending.fail_all(ClientFailure::background_task(
            "session supervisor exited unexpectedly",
        ));
        self.done.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending as never;

    use tokio::sync::oneshot;

    use super::*;
    use crate::client::pending::RegisterError;

    struct DropNotice(Option<oneshot::Sender<()>>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    async fn controlled_task(
        result: oneshot::Receiver<SessionTaskResult>,
        _notice: DropNotice,
    ) -> SessionTaskResult {
        result.await.unwrap_or_else(|_| {
            Err(ClientFailure::background_task(
                "test task controller was dropped",
            ))
        })
    }

    async fn never_task(_notice: DropNotice) -> SessionTaskResult {
        never().await
    }

    #[tokio::test]
    async fn receive_disconnect_fails_all_and_joins_siblings() {
        let pending = PendingRegistry::new();
        let mut first = pending.register("first".to_string()).unwrap();
        let mut second = pending.register("second".to_string()).unwrap();
        let (send_dropped_tx, send_dropped_rx) = oneshot::channel();
        let (event_dropped_tx, event_dropped_rx) = oneshot::channel();
        let (receive_result_tx, receive_result_rx) = oneshot::channel();
        let (receive_dropped_tx, _receive_dropped_rx) = oneshot::channel();
        let failure = ClientFailure::rpc_receive("connection lost");
        let session = ClientSession::spawn(
            pending.clone(),
            never_task(DropNotice(Some(send_dropped_tx))),
            controlled_task(receive_result_rx, DropNotice(Some(receive_dropped_tx))),
            never_task(DropNotice(Some(event_dropped_tx))),
        );

        receive_result_tx.send(Err(failure.clone())).unwrap();

        assert_eq!(first.receive().await.unwrap_err(), failure);
        assert_eq!(second.receive().await.unwrap_err(), failure);
        session.close().await;
        send_dropped_rx.await.unwrap();
        event_dropped_rx.await.unwrap();
        assert_eq!(pending.terminal_failure(), Some(failure.clone()));
        assert!(matches!(
            pending.register("late".to_string()),
            Err(RegisterError::Terminal(observed)) if observed == failure
        ));
    }

    #[tokio::test]
    async fn send_disconnect_fails_all_and_joins_siblings() {
        let pending = PendingRegistry::new();
        let mut lease = pending.register("request".to_string()).unwrap();
        let (send_result_tx, send_result_rx) = oneshot::channel();
        let (send_dropped_tx, _send_dropped_rx) = oneshot::channel();
        let (receive_dropped_tx, receive_dropped_rx) = oneshot::channel();
        let (event_dropped_tx, event_dropped_rx) = oneshot::channel();
        let failure = ClientFailure::rpc_send("connection lost");
        let session = ClientSession::spawn(
            pending.clone(),
            controlled_task(send_result_rx, DropNotice(Some(send_dropped_tx))),
            never_task(DropNotice(Some(receive_dropped_tx))),
            never_task(DropNotice(Some(event_dropped_tx))),
        );

        send_result_tx.send(Err(failure.clone())).unwrap();

        assert_eq!(lease.receive().await.unwrap_err(), failure);
        session.close().await;
        receive_dropped_rx.await.unwrap();
        event_dropped_rx.await.unwrap();
        assert_eq!(pending.terminal_failure(), Some(failure));
    }
}
