//! Generazione delle waveform in background: thread dedicato che riceve
//! `(path, content_hash, num_peaks)` da generare e li processa uno alla
//! volta — stesso modello di `proxy_worker` (coda seriale, un thread solo,
//! mai sul thread UI: la decodifica audio di un file lungo può richiedere
//! secondi, inaccettabile bloccando i frame). Il file di picchi finisce
//! nella cache globale su disco (`vv_media::waveform`, chiave
//! `content_hash`), quindi è riusabile da un altro progetto che
//! referenzia lo stesso file e sopravvive al riavvio dell'app.

use std::path::PathBuf;
use std::sync::mpsc;

struct Job {
    path: PathBuf,
    content_hash: u64,
    num_peaks: usize,
}

pub struct WaveformWorker {
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WaveformWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let handle = std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                // Già generato (es. stesso file importato in due progetti
                // diversi nella stessa sessione, o rimasto da una sessione
                // precedente): salta, non c'è nulla da rifare —
                // `generate_waveform` non fa questo controllo da sé.
                if vv_media::waveform::waveform_exists(job.content_hash) {
                    continue;
                }
                match vv_media::waveform::generate_waveform(
                    &job.path,
                    job.content_hash,
                    job.num_peaks,
                ) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        // Nessuna traccia audio: nessun picco da disegnare,
                        // e niente da scrivere — la timeline non chiede mai
                        // una waveform per un media senza audio.
                    }
                    Err(e) => {
                        eprintln!(
                            "[waveform_worker] generazione fallita per {}: {e}",
                            job.path.display()
                        );
                    }
                }
            }
        });
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Accoda `path` (chiave `content_hash`) per la generazione della
    /// waveform — non bloccante, ritorna subito.
    pub fn enqueue(&self, path: PathBuf, content_hash: u64, num_peaks: usize) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job {
                path,
                content_hash,
                num_peaks,
            });
        }
    }
}

impl Drop for WaveformWorker {
    fn drop(&mut self) {
        // `tx` va droppato *prima* di `join`: il thread esce dal suo
        // `while let Ok(...) = rx.recv()` solo quando l'ultimo mittente
        // sparisce — se aspettassimo che accadesse da sé alla fine di
        // questo scope (ordine di drop dei campi), `join()` bloccherebbe
        // per sempre aspettando un thread che a sua volta aspetta un
        // `tx` non ancora droppato.
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}