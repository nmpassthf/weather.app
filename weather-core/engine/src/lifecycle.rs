use tokio::sync::watch;

use crate::runtime::EngineExit;

#[derive(Clone)]
pub(crate) struct Cancellation {
    tx: watch::Sender<bool>,
}

impl Cancellation {
    pub(crate) fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self { tx }
    }

    pub(crate) fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub(crate) async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow_and_update() {
                return;
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct EngineControl {
    tx: watch::Sender<Option<EngineExit>>,
}

impl EngineControl {
    pub(crate) fn new() -> Self {
        let (tx, _) = watch::channel(None);
        Self { tx }
    }

    pub(crate) fn request_exit(&self, exit: EngineExit) {
        self.tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(exit);
                true
            }
        });
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<EngineExit>> {
        self.tx.subscribe()
    }
}

pub(crate) async fn wait_for_exit(rx: &mut watch::Receiver<Option<EngineExit>>) -> EngineExit {
    loop {
        if let Some(exit) = *rx.borrow_and_update() {
            return exit;
        }
        if rx.changed().await.is_err() {
            return EngineExit::Shutdown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_observed_even_when_already_cancelled() {
        let cancellation = Cancellation::new();
        cancellation.cancel();

        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn first_exit_request_wins() {
        let control = EngineControl::new();
        let mut rx = control.subscribe();
        control.request_exit(EngineExit::Restart);
        control.request_exit(EngineExit::Shutdown);

        assert_eq!(wait_for_exit(&mut rx).await, EngineExit::Restart);
    }
}
