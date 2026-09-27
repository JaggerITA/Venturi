//! Probing the files to import on several threads: probing a video costs
//! ~100 ms (see `vv_media::probe`), which on a multiple import would block
//! the UI for seconds.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use vv_core::MediaMeta;

pub type ProbeResult = Result<MediaMeta, String>;

struct Job {
    index: usize,
    path: PathBuf,
}

pub struct ImportWorker {
    rx: mpsc::Receiver<(usize, ProbeResult)>,
    /// Results that arrived but were not delivered yet: they are delivered
    /// in selection order, not completion order, or the media pool would
    /// end up in a different order on every import.
    buffer: Vec<Option<ProbeResult>>,
    paths: Vec<PathBuf>,
    next: usize,
    completed: usize,
}

impl ImportWorker {
    pub fn spawn(paths: Vec<PathBuf>, waker: crate::Waker) -> Self {
        let (job_tx, job_rx) = mpsc::channel();
        let (result_tx, rx) = mpsc::channel();
        for (index, path) in paths.iter().enumerate() {
            let _ = job_tx.send(Job {
                index,
                path: path.clone(),
            });
        }
        drop(job_tx);

        let job_rx = Arc::new(Mutex::new(job_rx));
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(paths.len().max(1));
        for _ in 0..threads {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let waker = waker.clone();
            std::thread::spawn(move || {
                loop {
                    let Ok(job) = ({
                        let rx = job_rx.lock().unwrap();
                        rx.recv()
                    }) else {
                        return;
                    };
                    let probed = vv_media::probe_media(&job.path);
                    if result_tx
                        .send((job.index, probed.map_err(|e| e.to_string())))
                        .is_err()
                    {
                        return;
                    }
                    waker.wake();
                }
            });
        }

        Self {
            rx,
            buffer: (0..paths.len()).map(|_| None).collect(),
            paths,
            next: 0,
            completed: 0,
        }
    }

    /// Probed media to hand to the pool now, in selection order: a result
    /// that arrived out of order stays queued until its turn comes.
    pub fn drain_ready(&mut self) -> Vec<(PathBuf, ProbeResult)> {
        for (index, result) in self.rx.try_iter() {
            self.buffer[index] = Some(result);
            self.completed += 1;
        }
        let mut ready = Vec::new();
        while self.next < self.buffer.len() {
            let Some(result) = self.buffer[self.next].take() else {
                break;
            };
            ready.push((self.paths[self.next].clone(), result));
            self.next += 1;
        }
        ready
    }

    pub fn is_finished(&self) -> bool {
        self.next == self.paths.len()
    }

    /// `(probed, total)` for the progress bar.
    pub fn progress(&self) -> (usize, usize) {
        (self.completed, self.paths.len())
    }
}
