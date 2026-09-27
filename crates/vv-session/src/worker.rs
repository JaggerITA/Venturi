//! Worker thread with a job queue.

use std::sync::mpsc;
use std::thread::JoinHandle;

/// On drop closes the queue, so the thread leaves `recv`, and waits for it
/// to finish.
pub struct Worker<J> {
    tx: Option<mpsc::Sender<J>>,
    handle: Option<JoinHandle<()>>,
}

impl<J: Send + 'static> Worker<J> {
    pub fn spawn(run: impl FnOnce(mpsc::Receiver<J>) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx: Some(tx),
            handle: Some(std::thread::spawn(move || run(rx))),
        }
    }

    /// `false` if the thread is gone.
    pub fn send(&self, job: J) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }
}

impl<J> Drop for Worker<J> {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
