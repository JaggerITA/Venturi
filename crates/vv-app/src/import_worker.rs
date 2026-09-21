//! Probe dei file da importare su più thread: il probe di un video costa
//! ~100 ms (vedi `vv_media::probe`), che su un import multiplo bloccherebbe
//! la UI per secondi.

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use vv_core::MediaMeta;

pub type ProbeResult = Result<MediaMeta, String>;

struct Job {
    index: usize,
    path: PathBuf,
}

pub struct ImportWorker {
    rx: mpsc::Receiver<(usize, ProbeResult)>,
    /// Risultati arrivati ma non ancora consegnati: si consegnano in
    /// ordine di selezione, non di completamento, o il media pool
    /// finirebbe in un ordine diverso a ogni import.
    buffer: Vec<Option<ProbeResult>>,
    paths: Vec<PathBuf>,
    next: usize,
    completed: usize,
}

impl ImportWorker {
    pub fn spawn(paths: Vec<PathBuf>, is_image: fn(&std::path::Path) -> bool) -> Self {
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
            std::thread::spawn(move || loop {
                let Ok(job) = ({
                    let rx = job_rx.lock().unwrap();
                    rx.recv()
                }) else {
                    return;
                };
                let probed = if is_image(&job.path) {
                    vv_media::probe_image(&job.path)
                } else {
                    vv_media::probe(&job.path)
                };
                if result_tx
                    .send((job.index, probed.map_err(|e| e.to_string())))
                    .is_err()
                {
                    return;
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

    /// Media probati da consegnare al pool adesso, in ordine di selezione:
    /// un risultato arrivato fuori ordine resta in coda finché non tocca a
    /// lui.
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

    /// `(probati, totali)` per la barra di avanzamento.
    pub fn progress(&self) -> (usize, usize) {
        (self.completed, self.paths.len())
    }
}
