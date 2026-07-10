use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::event::{self, Event};
use tokio::{sync::mpsc, task::JoinHandle};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) struct TerminalInput {
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl TerminalInput {
    pub fn spawn() -> (Self, mpsc::UnboundedReceiver<Result<Event, String>>) {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (sender, receiver) = mpsc::unbounded_channel();
        let task = tokio::task::spawn_blocking(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match event::poll(INPUT_POLL_INTERVAL) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if sender.send(Ok(event)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string()));
                            return;
                        }
                    },
                    Ok(false) => {}
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });
        (
            Self {
                stop,
                task: Some(task),
            },
            receiver,
        )
    }

    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        // A cancelled controller cannot await the blocking worker, but the
        // bounded poll observes this flag within one INPUT_POLL_INTERVAL.
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn dropping_owner_requests_worker_shutdown() {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (stopped, stopped_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            while !worker_stop.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            let _ = stopped.send(());
        });
        let input = TerminalInput {
            stop,
            task: Some(task),
        };

        drop(input);

        tokio::time::timeout(Duration::from_secs(1), stopped_rx)
            .await
            .unwrap()
            .unwrap();
    }
}
