//! Media pool thumbnails in the background: opening a file and seeking
//! costs tens of ms per media, too much on the UI thread with a multiple
//! import.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::worker::Worker;

/// Width in pixels of the generated thumbnails.
pub const THUMBNAIL_WIDTH: u32 = 96;

struct Job {
    path: PathBuf,
    content_hash: u64,
    duration_secs: f64,
}

pub struct ThumbnailWorker {
    worker: Worker<Job>,
    rx: mpsc::Receiver<(u64, Option<vv_media::Thumbnail>)>,
    pending: usize,
}

impl ThumbnailWorker {
    pub fn spawn() -> Self {
        let (result_tx, rx) = mpsc::channel();
        let worker = Worker::spawn(move |job_rx: mpsc::Receiver<Job>| {
            while let Ok(job) = job_rx.recv() {
                let thumb = vv_media::generate_thumbnail(&job.path, job.duration_secs, THUMBNAIL_WIDTH)
                    .inspect_err(|e| {
                        eprintln!(
                            "[thumbnail_worker] thumbnail failed for {}: {e}",
                            job.path.display()
                        )
                    })
                    .ok();
                if result_tx.send((job.content_hash, thumb)).is_err() {
                    return;
                }
            }
        });
        Self {
            worker,
            rx,
            pending: 0,
        }
    }

    pub fn enqueue(&mut self, path: PathBuf, content_hash: u64, duration_secs: f64) {
        if self.worker.send(Job {
            path,
            content_hash,
            duration_secs,
        }) {
            self.pending += 1;
        }
    }

    /// Thumbnails completed since the last call (`None` if they failed).
    pub fn drain(&mut self) -> Vec<(u64, Option<vv_media::Thumbnail>)> {
        let ready: Vec<_> = self.rx.try_iter().collect();
        self.pending -= ready.len();
        ready
    }

    pub fn has_pending(&self) -> bool {
        self.pending > 0
    }
}
