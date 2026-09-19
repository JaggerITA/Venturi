//! Thread di lavoro con una coda di job.

use std::sync::mpsc;
use std::thread::JoinHandle;

/// Al drop chiude la coda, così il thread esce da `recv`, e ne aspetta la
/// fine.
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

    /// `false` se il thread non c'è più.
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
