//! Proxy generation on a dedicated thread, one at a time: kept separate from
//! `render_ahead`'s worker so it does not slow down playback decoding.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use vv_media::proxy::ProxyQuality;

use crate::worker::Worker;

struct Job {
    path: PathBuf,
    content_hash: u64,
    duration_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProxyState {
    Queued,
    /// Fraction completed, 0..=1.
    Generating(f32),
    Ready,
    Failed,
}

#[derive(Default)]
struct Shared {
    states: Mutex<HashMap<u64, ProxyState>>,
    paused: Mutex<bool>,
    resume: Condvar,
    shutdown: AtomicBool,
}

impl Shared {
    fn set_state(&self, content_hash: u64, state: ProxyState) {
        self.states.lock().unwrap().insert(content_hash, state);
    }

    /// Blocks while paused; `false` if the worker must terminate.
    fn wait_while_paused(&self) -> bool {
        let mut paused = self.paused.lock().unwrap();
        while *paused && !self.shutdown.load(Ordering::Relaxed) {
            paused = self.resume.wait(paused).unwrap();
        }
        !self.shutdown.load(Ordering::Relaxed)
    }
}

/// Overall progress of the session queue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxyProgress {
    /// Ready or failed: there is nothing left to do for them.
    pub finished: usize,
    pub total: usize,
    pub fraction: f32,
}

/// Bound to one quality: changing it means spawning a new worker.
pub struct ProxyWorker {
    shared: Arc<Shared>,
    worker: Worker<Job>,
    quality: ProxyQuality,
}

impl ProxyWorker {
    pub fn spawn(quality: ProxyQuality) -> Self {
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = Worker::spawn(move |rx: mpsc::Receiver<Job>| {
            let shared = worker_shared;
            while let Ok(job) = rx.recv() {
                if !shared.wait_while_paused() {
                    return;
                }
                if vv_media::proxy::proxy_exists(job.content_hash, quality) {
                    shared.set_state(job.content_hash, ProxyState::Ready);
                    continue;
                }
                shared.set_state(job.content_hash, ProxyState::Generating(0.0));
                let total = job.duration_frames.max(1) as f32;
                let mut last_reported = 0.0f32;
                let result = vv_media::proxy::generate_proxy(
                    &job.path,
                    job.content_hash,
                    quality,
                    |frames| {
                        let fraction = (frames as f32 / total).min(1.0);
                        // Updating the lock on every frame is pointless: the UI reads at ~60Hz.
                        if fraction - last_reported >= 0.005 {
                            last_reported = fraction;
                            shared.set_state(job.content_hash, ProxyState::Generating(fraction));
                        }
                        shared.wait_while_paused()
                    },
                );
                match result {
                    Ok(_) => shared.set_state(job.content_hash, ProxyState::Ready),
                    Err(vv_media::MediaError::Cancelled) => return,
                    Err(e) => {
                        eprintln!(
                            "[proxy_worker] generation failed for {}: {e}",
                            job.path.display()
                        );
                        shared.set_state(job.content_hash, ProxyState::Failed);
                    }
                }
            }
        });
        Self { shared, worker, quality }
    }

    /// Queues `path` (keyed by `content_hash`) for proxy generation —
    /// non-blocking, returns immediately. A media already queued in this
    /// session is not queued again.
    pub fn enqueue(&self, path: PathBuf, content_hash: u64, duration_frames: u64) {
        {
            let mut states = self.shared.states.lock().unwrap();
            if matches!(
                states.get(&content_hash),
                Some(ProxyState::Queued | ProxyState::Generating(_) | ProxyState::Ready)
            ) {
                return;
            }
            let state = if vv_media::proxy::proxy_exists(content_hash, self.quality) {
                ProxyState::Ready
            } else {
                ProxyState::Queued
            };
            states.insert(content_hash, state);
            if state == ProxyState::Ready {
                return;
            }
        }
        self.worker.send(Job {
            path,
            content_hash,
            duration_frames,
        });
    }

    pub fn quality(&self) -> ProxyQuality {
        self.quality
    }

    pub fn state(&self, content_hash: u64) -> Option<ProxyState> {
        self.shared.states.lock().unwrap().get(&content_hash).copied()
    }

    pub fn progress(&self) -> ProxyProgress {
        let states = self.shared.states.lock().unwrap();
        let mut finished = 0;
        let mut partial = 0.0;
        for state in states.values() {
            match state {
                ProxyState::Ready | ProxyState::Failed => finished += 1,
                ProxyState::Generating(f) => partial += f,
                ProxyState::Queued => {}
            }
        }
        let total = states.len();
        let fraction = if total == 0 {
            1.0
        } else {
            (finished as f32 + partial) / total as f32
        };
        ProxyProgress {
            finished,
            total,
            fraction,
        }
    }

    pub fn is_paused(&self) -> bool {
        *self.shared.paused.lock().unwrap()
    }

    pub fn set_paused(&self, paused: bool) {
        *self.shared.paused.lock().unwrap() = paused;
        self.shared.resume.notify_all();
    }
}

impl Drop for ProxyWorker {
    fn drop(&mut self) {
        // Also interrupts an encode in progress or paused before `worker`
        // waits for the thread: otherwise it would wait for the end of the file.
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.set_paused(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cond() {
            assert!(Instant::now() < deadline, "timeout: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pause_holds_the_queue_and_resume_completes_it() {
        let dir = std::env::temp_dir().join("vv-app-proxy-worker-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );
        let content_hash = 0x5EED_0001;
        let _ = std::fs::remove_file(vv_media::proxy::proxy_path_for(content_hash, ProxyQuality::Low));

        let worker = ProxyWorker::spawn(ProxyQuality::Low);
        worker.set_paused(true);
        worker.enqueue(path.clone(), content_hash, 25);
        worker.enqueue(path, content_hash, 25);
        assert_eq!(worker.progress().total, 1, "un media riaccodato non conta due volte");

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(worker.state(content_hash), Some(ProxyState::Queued));
        assert_eq!(worker.progress().finished, 0);

        worker.set_paused(false);
        wait_until(
            || worker.state(content_hash) == Some(ProxyState::Ready),
            "proxy mai pronto dopo la ripresa",
        );
        let progress = worker.progress();
        assert_eq!((progress.finished, progress.total), (1, 1));
        assert_eq!(progress.fraction, 1.0);
    }
}
