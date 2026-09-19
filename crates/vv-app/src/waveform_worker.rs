//! Generazione waveform su un thread dedicato, una alla volta: decodificare
//! l'audio di un file lungo richiede secondi.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::worker::Worker;

struct Job {
    path: PathBuf,
    content_hash: u64,
    audio_streams: usize,
    num_peaks: usize,
}

pub struct WaveformWorker {
    worker: Worker<Job>,
    /// `(content_hash, stream_index)` delle waveform pronte su disco.
    ready_rx: mpsc::Receiver<(u64, usize)>,
}

impl WaveformWorker {
    pub fn spawn() -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = Worker::spawn(move |rx: mpsc::Receiver<Job>| {
            while let Ok(job) = rx.recv() {
                for key in generate(&job) {
                    if ready_tx.send(key).is_err() {
                        return;
                    }
                }
            }
        });
        Self { worker, ready_rx }
    }

    /// Waveform completate dall'ultima chiamata.
    pub fn drain_ready(&self) -> Vec<(u64, usize)> {
        self.ready_rx.try_iter().collect()
    }

    /// Accoda la generazione delle waveform di tutti gli stream audio di
    /// `path` che non sono già su disco — non bloccante, ritorna subito.
    pub fn enqueue(&self, path: PathBuf, content_hash: u64, audio_streams: usize, num_peaks: usize) {
        self.worker.send(Job {
            path,
            content_hash,
            audio_streams,
            num_peaks,
        });
    }
}


/// Genera in una passata le waveform mancanti di `job`; restituisce le
/// chiavi di quelle pronte su disco.
fn generate(job: &Job) -> Vec<(u64, usize)> {
    let (ready, missing): (Vec<usize>, Vec<usize>) = (0..job.audio_streams)
        .partition(|&i| vv_media::waveform::waveform_exists(job.content_hash, i));
    let mut ready: Vec<(u64, usize)> = ready.into_iter().map(|i| (job.content_hash, i)).collect();
    if missing.is_empty() {
        return ready;
    }
    match vv_media::generate_waveforms(&job.path, job.content_hash, &missing, job.num_peaks) {
        Ok(waveforms) => ready.extend(
            missing
                .iter()
                .zip(waveforms)
                .filter(|(_, w)| w.is_some())
                .map(|(&i, _)| (job.content_hash, i)),
        ),
        Err(e) => eprintln!(
            "[waveform_worker] generation failed for {}: {e}",
            job.path.display()
        ),
    }
    ready
}
