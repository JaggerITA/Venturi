//! Generazione proxy in background (REFACTOR_PIPELINE.md proxy):
//! thread dedicato che riceve `(path, content_hash)` da generare e li
//! processa uno alla volta — separato dal worker di `render_ahead` (che
//! bufferizza per il playback, non deve competere con un encode
//! potenzialmente lungo) e dal thread UI (l'encode di un intero file può
//! richiedere secondi, inaccettabile bloccando i frame). Coda seriale,
//! non un pool: come `render_ahead`, un thread solo si è mostrato
//! sufficiente finora, e più thread in concorrenza sulla stessa CPU
//! rallenterebbero anche il decode "vero" per il playback.

use std::path::PathBuf;
use std::sync::mpsc;

struct Job {
    path: PathBuf,
    content_hash: u64,
}

pub struct ProxyWorker {
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProxyWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let handle = std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                // Già generato (es. stesso file importato in due
                // progetti diversi nella stessa sessione, o rimasto da
                // una sessione precedente): salta, non c'è nulla da
                // rifare — `generate_proxy` non fa questo controllo da
                // sé (utile poterlo forzare in altri contesti).
                if vv_media::proxy::proxy_exists(job.content_hash) {
                    continue;
                }
                if let Err(e) = vv_media::proxy::generate_proxy(&job.path, job.content_hash) {
                    eprintln!(
                        "[proxy_worker] generazione fallita per {}: {e}",
                        job.path.display()
                    );
                }
            }
        });
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Accoda `path` (chiave `content_hash`) per la generazione del
    /// proxy — non bloccante, ritorna subito.
    pub fn enqueue(&self, path: PathBuf, content_hash: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job { path, content_hash });
        }
    }
}

impl Drop for ProxyWorker {
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
