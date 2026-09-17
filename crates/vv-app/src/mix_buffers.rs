//! Buffer audio già nel formato del mixer, per `(path, audio_stream_index)`.
//! Decodifica + resample su un thread dedicato: finché un buffer non è
//! pronto la clip suona silenzio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

type Key = (PathBuf, usize);
type Ready = (Key, Option<Arc<Vec<f32>>>);

pub struct MixBufferCache {
    /// `None` = in decodifica, oppure senza audio a quell'indice.
    entries: HashMap<Key, Option<Arc<Vec<f32>>>>,
    job_tx: Option<mpsc::Sender<Key>>,
    ready_rx: mpsc::Receiver<Ready>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MixBufferCache {
    pub fn spawn(sample_rate: u32, channels: u16) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Key>();
        let (ready_tx, ready_rx) = mpsc::channel::<Ready>();
        let handle = std::thread::spawn(move || {
            while let Ok((path, stream)) = job_rx.recv() {
                let buffer = match vv_media::decode_audio_track(&path, stream) {
                    Ok(Some(audio)) => Some(Arc::new(vv_audio::mixer::prepare_mix_buffer(
                        &audio.samples,
                        audio.sample_rate,
                        audio.channels,
                        sample_rate,
                        channels,
                    ))),
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!("[mix_buffers] decodifica fallita per {}: {e}", path.display());
                        None
                    }
                };
                if ready_tx.send(((path, stream), buffer)).is_err() {
                    break;
                }
            }
        });
        Self {
            entries: HashMap::new(),
            job_tx: Some(job_tx),
            ready_rx,
            handle: Some(handle),
        }
    }

    /// Non bloccante: se il buffer non c'è ne accoda la decodifica (una
    /// volta sola per chiave).
    pub fn get_or_request(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        let key = (path.to_path_buf(), stream);
        if let Some(entry) = self.entries.get(&key) {
            return entry.clone();
        }
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(key.clone());
        }
        self.entries.insert(key, None);
        None
    }

    /// Raccoglie i buffer pronti; `true` se ne è arrivato almeno uno (lo
    /// snapshot del mixer va ricostruito).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, buffer)) = self.ready_rx.try_recv() {
            changed |= buffer.is_some();
            self.entries.insert(key, buffer);
        }
        changed
    }

    #[cfg(test)]
    pub fn has_pending(&mut self) -> bool {
        self.poll();
        self.entries.values().any(Option::is_none)
    }
}

impl Drop for MixBufferCache {
    fn drop(&mut self) {
        // Senza mittente il worker esce da `recv`, altrimenti `join` resta appeso.
        self.job_tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_ready(cache: &mut MixBufferCache) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cache.poll() {
            assert!(std::time::Instant::now() < deadline, "decodifica mai arrivata");
            std::thread::yield_now();
        }
    }

    #[test]
    fn buffers_are_decoded_in_background_per_path_and_stream() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two_streams.mkv");
        // Stream 0 mono 44.1 kHz da 1s, stream 1 stereo 48 kHz da 0.5s.
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000:duration=0.5",
                "-map",
                "0:a",
                "-map",
                "1:a",
                "-ac:1",
                "2",
                "-c:a",
                "pcm_f32le",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut cache = MixBufferCache::spawn(48_000, 2);
        assert!(cache.get_or_request(&path, 0).is_none());
        assert!(cache.get_or_request(&path, 1).is_none());
        wait_ready(&mut cache);
        if cache.get_or_request(&path, 0).is_none() || cache.get_or_request(&path, 1).is_none() {
            wait_ready(&mut cache);
        }

        let s0 = cache.get_or_request(&path, 0).expect("stream 0 pronto");
        let s1 = cache.get_or_request(&path, 1).expect("stream 1 pronto");
        let frames = |b: &Vec<f32>| b.len() / 2;
        assert!((frames(&s0) as i64 - 48_000).abs() < 100, "s0={}", frames(&s0));
        assert!((frames(&s1) as i64 - 24_000).abs() < 100, "s1={}", frames(&s1));
        assert!(cache.get_or_request(&path, 5).is_none(), "stream inesistente");
    }
}
