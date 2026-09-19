//! Generazione proxy su un thread dedicato, una alla volta: separata dal
//! worker di `render_ahead` per non rallentare il decode del playback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use crate::worker::Worker;

struct Job {
    path: PathBuf,
    content_hash: u64,
    duration_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProxyState {
    Queued,
    /// Frazione completata, 0..=1.
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

    /// Blocca finché in pausa; `false` se il worker deve terminare.
    fn wait_while_paused(&self) -> bool {
        let mut paused = self.paused.lock().unwrap();
        while *paused && !self.shutdown.load(Ordering::Relaxed) {
            paused = self.resume.wait(paused).unwrap();
        }
        !self.shutdown.load(Ordering::Relaxed)
    }
}

/// Avanzamento complessivo della coda della sessione.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxyProgress {
    /// Pronti o falliti: non c'è più nulla da fare per loro.
    pub finished: usize,
    pub total: usize,
    pub fraction: f32,
}

pub struct ProxyWorker {
    shared: Arc<Shared>,
    worker: Worker<Job>,
}

impl ProxyWorker {
    pub fn spawn() -> Self {
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = Worker::spawn(move |rx: mpsc::Receiver<Job>| {
            let shared = worker_shared;
            while let Ok(job) = rx.recv() {
                if !shared.wait_while_paused() {
                    return;
                }
                if vv_media::proxy::proxy_exists(job.content_hash) {
                    shared.set_state(job.content_hash, ProxyState::Ready);
                    continue;
                }
                shared.set_state(job.content_hash, ProxyState::Generating(0.0));
                let total = job.duration_frames.max(1) as f32;
                let mut last_reported = 0.0f32;
                let result = vv_media::proxy::generate_proxy(
                    &job.path,
                    job.content_hash,
                    |frames| {
                        let fraction = (frames as f32 / total).min(1.0);
                        // Aggiornare il lock a ogni frame è inutile: la UI legge a ~60Hz.
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
                            "[proxy_worker] generazione fallita per {}: {e}",
                            job.path.display()
                        );
                        shared.set_state(job.content_hash, ProxyState::Failed);
                    }
                }
            }
        });
        Self { shared, worker }
    }

    /// Accoda `path` (chiave `content_hash`) per la generazione del
    /// proxy — non bloccante, ritorna subito. Un media già accodato in
    /// questa sessione non viene riaccodato.
    pub fn enqueue(&self, path: PathBuf, content_hash: u64, duration_frames: u64) {
        {
            let mut states = self.shared.states.lock().unwrap();
            if matches!(
                states.get(&content_hash),
                Some(ProxyState::Queued | ProxyState::Generating(_) | ProxyState::Ready)
            ) {
                return;
            }
            let state = if vv_media::proxy::proxy_exists(content_hash) {
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
        // Interrompe anche un encode in corso o in pausa prima che `worker`
        // aspetti il thread: altrimenti aspetterebbe la fine del file.
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
        let _ = std::fs::remove_file(vv_media::proxy::proxy_path_for(content_hash));

        let worker = ProxyWorker::spawn();
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
