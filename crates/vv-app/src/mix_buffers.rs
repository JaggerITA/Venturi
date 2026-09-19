//! Buffer audio già nel formato del mixer, per `(path, audio_stream_index)`.
//! Decodifica + resample su un thread dedicato. Il buffer viene pubblicato
//! parziale mentre cresce: il mixer suona l'inizio della traccia prima che
//! la decodifica finisca, oltre la parte pronta c'è silenzio.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use crate::worker::Worker;

type Key = (PathBuf, usize);

struct Ready {
    key: Key,
    buffer: Option<Arc<Vec<f32>>>,
    done: bool,
}

/// Secondi di audio dopo cui esce la prima pubblicazione; le successive a
/// ogni raddoppio, così le copie del buffer costano ~2x la decodifica.
const FIRST_PUBLISH_SECS: usize = 1;

pub struct MixBufferCache {
    /// `None` = nessun campione ancora, oppure senza audio a quell'indice.
    entries: HashMap<Key, Option<Arc<Vec<f32>>>>,
    in_progress: HashSet<Key>,
    worker: Worker<(Key, bool)>,
    ready_rx: mpsc::Receiver<Ready>,
}

impl MixBufferCache {
    pub fn spawn(sample_rate: u32, channels: u16) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel::<Ready>();
        let worker = Worker::spawn(move |job_rx: mpsc::Receiver<(Key, bool)>| {
            let mut queue = VecDeque::new();
            let mut done = HashSet::new();
            loop {
                if queue.is_empty() {
                    match job_rx.recv() {
                        Ok(job) => enqueue(&mut queue, job),
                        Err(_) => break,
                    }
                }
                while let Ok(job) = job_rx.try_recv() {
                    enqueue(&mut queue, job);
                }
                let Some(key) = queue.pop_front() else {
                    continue;
                };
                // Gli altri stream in coda dello stesso file vanno nella
                // stessa passata: arrivano tutti subito invece che in fila.
                let mut streams = vec![key.1];
                queue.retain(|(path, stream)| {
                    let same_file = *path == key.0;
                    if same_file && !streams.contains(stream) {
                        streams.push(*stream);
                    }
                    !same_file
                });
                // Una richiesta prioritaria duplica una già in coda.
                streams.retain(|&stream| done.insert((key.0.clone(), stream)));
                if streams.is_empty() {
                    continue;
                }
                if !decode_progressively(&key.0, &streams, sample_rate, channels, &ready_tx) {
                    break;
                }
            }
        });
        Self {
            entries: HashMap::new(),
            in_progress: HashSet::new(),
            worker,
            ready_rx,
        }
    }

    /// Non bloccante: se il buffer non c'è ne accoda la decodifica (una
    /// volta sola per chiave). Il buffer restituito può essere parziale.
    pub fn get_or_request(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        self.request(path, stream, false)
    }

    /// Come `get_or_request`, ma la decodifica passa davanti a quelle in coda
    /// (non interrompe quella in corso).
    pub fn get_or_request_first(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        self.request(path, stream, true)
    }

    fn request(&mut self, path: &Path, stream: usize, first: bool) -> Option<Arc<Vec<f32>>> {
        let key = (path.to_path_buf(), stream);
        if let Some(entry) = self.entries.get(&key) {
            // Già in coda: ripetuta davanti, il worker scarta il duplicato.
            if first && entry.is_none() && self.in_progress.contains(&key) {
                self.worker.send((key.clone(), true));
            }
            return entry.clone();
        }
        self.worker.send((key.clone(), first));
        self.in_progress.insert(key.clone());
        self.entries.insert(key, None);
        None
    }

    /// Raccoglie i buffer pronti (anche parziali); `true` se ne è arrivato
    /// almeno uno (lo snapshot del mixer va ricostruito).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ready) = self.ready_rx.try_recv() {
            if ready.done {
                self.in_progress.remove(&ready.key);
            }
            if ready.buffer.is_some() {
                changed = true;
                self.entries.insert(ready.key, ready.buffer);
            }
        }
        changed
    }

    #[cfg(test)]
    pub fn has_pending(&mut self) -> bool {
        self.poll();
        !self.in_progress.is_empty()
    }
}

fn enqueue(queue: &mut VecDeque<Key>, (key, first): (Key, bool)) {
    if first {
        queue.push_front(key);
    } else {
        queue.push_back(key);
    }
}

/// `false` se il ricevente non c'è più (il worker deve uscire).
fn decode_progressively(
    path: &Path,
    streams: &[usize],
    sample_rate: u32,
    channels: u16,
    ready_tx: &mpsc::Sender<Ready>,
) -> bool {
    let key = |slot: usize| (path.to_path_buf(), streams[slot]);
    let first_publish = FIRST_PUBLISH_SECS * sample_rate as usize * channels as usize;
    let mut buffers = vec![Vec::new(); streams.len()];
    let mut next_publish = vec![first_publish; streams.len()];
    let mut receiver_gone = false;
    let result = vv_media::decode_audio_streams_streaming(
        path,
        streams,
        Some(sample_rate),
        |slot, src_channels, chunk| {
            let buffer = &mut buffers[slot];
            vv_audio::mixer::remix_channels_into(chunk, src_channels, channels, buffer);
            if buffer.len() < next_publish[slot] {
                return ControlFlow::Continue(());
            }
            next_publish[slot] = buffer.len() * 2;
            let ready = Ready {
                key: key(slot),
                buffer: Some(Arc::new(buffer.clone())),
                done: false,
            };
            if ready_tx.send(ready).is_err() {
                receiver_gone = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    );
    if receiver_gone {
        return false;
    }
    let formats = result.unwrap_or_else(|e| {
        eprintln!("[mix_buffers] decodifica fallita per {}: {e}", path.display());
        vec![None; streams.len()]
    });
    for (slot, buffer) in buffers.into_iter().enumerate() {
        let buffer = (formats[slot].is_some() && !buffer.is_empty()).then(|| Arc::new(buffer));
        let ready = Ready {
            key: key(slot),
            buffer,
            done: true,
        };
        if ready_tx.send(ready).is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_done(cache: &mut MixBufferCache) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while cache.has_pending() {
            assert!(std::time::Instant::now() < deadline, "decodifica mai finita");
            std::thread::yield_now();
        }
    }

    #[test]
    fn buffers_are_decoded_in_background_per_path_and_stream() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two_streams.mkv");
        // Stream 0 mono 44.1 kHz da 1s, stream 1 stereo 48 kHz da 0.5s.
        vv_media::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );

        let mut cache = MixBufferCache::spawn(48_000, 2);
        assert!(cache.get_or_request(&path, 0).is_none());
        assert!(cache.get_or_request(&path, 1).is_none());
        wait_done(&mut cache);

        let s0 = cache.get_or_request(&path, 0).expect("stream 0 pronto");
        let s1 = cache.get_or_request(&path, 1).expect("stream 1 pronto");
        let frames = |b: &Vec<f32>| b.len() / 2;
        assert!((frames(&s0) as i64 - 48_000).abs() < 100, "s0={}", frames(&s0));
        assert!((frames(&s1) as i64 - 24_000).abs() < 100, "s1={}", frames(&s1));
        assert!(cache.get_or_request(&path, 5).is_none(), "stream inesistente");
    }

    #[test]
    fn a_long_track_is_published_partially_before_decoding_ends() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("long.wav");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=20",
            ],
            &path,
        );

        let mut cache = MixBufferCache::spawn(48_000, 2);
        cache.get_or_request(&path, 0);
        let mut lengths = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while cache.has_pending() {
            assert!(std::time::Instant::now() < deadline, "decodifica mai finita");
            if let Some(buffer) = cache.get_or_request(&path, 0)
                && lengths.last() != Some(&buffer.len())
            {
                lengths.push(buffer.len());
            }
            std::thread::yield_now();
        }
        let full = cache.get_or_request(&path, 0).unwrap().len();
        assert_eq!(full, 20 * 48_000 * 2);
        assert!(
            lengths.first().is_some_and(|&first| first < full),
            "attesa almeno una pubblicazione parziale: {lengths:?}"
        );
    }

    #[test]
    fn a_priority_request_jumps_the_queue() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = ["queue_a.wav", "queue_b.wav", "queue_c.wav"]
            .iter()
            .map(|name| {
                let path = dir.join(name);
                vv_media::test_support::ffmpeg(
                    &[
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=48000:duration=30",
                    ],
                    &path,
                );
                path
            })
            .collect();

        let mut cache = MixBufferCache::spawn(48_000, 2);
        cache.get_or_request(&paths[0], 0);
        cache.get_or_request(&paths[1], 0);
        cache.get_or_request_first(&paths[2], 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "decodifica mai arrivata");
            cache.poll();
            if cache.get_or_request(&paths[2], 0).is_some() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            cache.get_or_request(&paths[1], 0).is_none(),
            "la richiesta prioritaria doveva passare davanti a quella in coda"
        );
    }

    #[test]
    fn every_stream_of_a_file_starts_playing_before_any_of_them_is_fully_decoded() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("three_streams_long.mkv");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=120",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=660:sample_rate=48000:duration=120",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000:duration=120",
                "-map",
                "0:a",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-c:a",
                "aac",
            ],
            &path,
        );

        let mut cache = MixBufferCache::spawn(48_000, 2);
        for stream in 0..3 {
            cache.get_or_request_first(&path, stream);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < deadline, "decodifica mai arrivata");
            cache.poll();
            if (0..3).all(|stream| cache.get_or_request(&path, stream).is_some()) {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(cache.in_progress.len(), 3, "nessuno stream doveva essere già finito");
        let full = 120 * 48_000 * 2;
        assert!((0..3).all(|stream| cache.get_or_request(&path, stream).unwrap().len() < full));
    }
}
