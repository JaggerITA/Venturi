//! Miniature del media pool in background: aprire il file e fare seek
//! costa decine di ms per media, troppo sul thread UI con un import
//! multiplo.

use std::path::PathBuf;
use std::sync::mpsc;

/// Larghezza in pixel delle miniature generate.
pub const THUMBNAIL_WIDTH: u32 = 96;

struct Job {
    path: PathBuf,
    content_hash: u64,
    duration_secs: f64,
}

pub struct ThumbnailWorker {
    tx: Option<mpsc::Sender<Job>>,
    rx: mpsc::Receiver<(u64, Option<vv_media::Thumbnail>)>,
    pending: usize,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ThumbnailWorker {
    pub fn spawn() -> Self {
        let (tx, job_rx) = mpsc::channel::<Job>();
        let (result_tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let thumb = vv_media::generate_thumbnail(&job.path, job.duration_secs, THUMBNAIL_WIDTH)
                    .inspect_err(|e| {
                        eprintln!(
                            "[thumbnail_worker] miniatura fallita per {}: {e}",
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
            tx: Some(tx),
            rx,
            pending: 0,
            handle: Some(handle),
        }
    }

    pub fn enqueue(&mut self, path: PathBuf, content_hash: u64, duration_secs: f64) {
        if let Some(tx) = &self.tx
            && tx
                .send(Job {
                    path,
                    content_hash,
                    duration_secs,
                })
                .is_ok()
        {
            self.pending += 1;
        }
    }

    /// Miniature completate dall'ultima chiamata (`None` se fallite).
    pub fn drain(&mut self) -> Vec<(u64, Option<vv_media::Thumbnail>)> {
        let ready: Vec<_> = self.rx.try_iter().collect();
        self.pending -= ready.len();
        ready
    }

    pub fn has_pending(&self) -> bool {
        self.pending > 0
    }
}

impl Drop for ThumbnailWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
