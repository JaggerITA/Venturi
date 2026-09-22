//! Audio buffers already in the mixer's format, per `(path, audio_stream_index)`.
//! Decode + resample on a dedicated thread. The buffer is published
//! partially while it grows: the mixer plays the start of the track before
//! the decode finishes, past the ready part there is silence.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use vv_core::{ClipSource, MediaId, Project};

use crate::worker::Worker;

type Key = (PathBuf, usize);

struct Ready {
    key: Key,
    buffer: Option<Arc<Vec<f32>>>,
    done: bool,
}

/// Seconds of audio after which the first publication goes out; the following
/// ones at every doubling, so the buffer copies cost ~2x the decode.
const FIRST_PUBLISH_SECS: usize = 1;

pub struct MixBufferCache {
    /// `None` = no samples yet, or no audio at that index.
    entries: HashMap<Key, Option<Arc<Vec<f32>>>>,
    in_progress: HashSet<Key>,
    worker: Worker<(Key, bool)>,
    ready_rx: mpsc::Receiver<Ready>,
    sample_rate: u32,
    channels: u16,
    /// Already computed mixdowns of a compound clip, keyed by `MediaId` +
    /// the `content_hash` they were computed with: see
    /// `get_or_compute_compound`.
    compound: HashMap<MediaId, (u64, Arc<Vec<f32>>)>,
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
                // The other queued streams of the same file go in the
                // same pass: they all arrive at once instead of in line.
                let mut streams = vec![key.1];
                queue.retain(|(path, stream)| {
                    let same_file = *path == key.0;
                    if same_file && !streams.contains(stream) {
                        streams.push(*stream);
                    }
                    !same_file
                });
                // A priority request duplicates one already queued.
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
            sample_rate,
            channels,
            compound: HashMap::new(),
        }
    }

    /// Non-blocking: if the buffer is not there it queues its decode (once
    /// per key only). The returned buffer may be partial.
    pub fn get_or_request(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        self.request(path, stream, false)
    }

    /// Like `get_or_request`, but the decode jumps ahead of the queued ones
    /// (it does not interrupt the one in progress).
    pub fn get_or_request_first(&mut self, path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        self.request(path, stream, true)
    }

    fn request(&mut self, path: &Path, stream: usize, first: bool) -> Option<Arc<Vec<f32>>> {
        let key = (path.to_path_buf(), stream);
        if let Some(entry) = self.entries.get(&key) {
            // Already queued: repeated at the front, the worker discards the duplicate.
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

    /// The mixdown of the nested timeline of a compound clip, at this
    /// cache's `sample_rate`/`channels` — treated from there on like an
    /// already decoded file. Cached by `content_hash`: `None` until even
    /// a single one of the audio clips involved (at any nesting depth) has
    /// a ready buffer — a mixdown missing a piece, once cached, would stay
    /// wrong until `content_hash` changes again. It does not go through
    /// `MixSnapshot::from_timeline` (which would take two closures both
    /// mutable on `self`, in conflict): it builds the `MixClip`s by hand
    /// with `vv_audio::mixer::mix_clip_from`, the same function
    /// `from_timeline` uses.
    pub fn get_or_compute_compound(&mut self, project: &Project, media_id: MediaId) -> Option<Arc<Vec<f32>>> {
        self.get_or_compute_compound_at_depth(project, media_id, 0)
    }

    /// Limit on the nesting depth: since the project timeline is in the
    /// media pool too, dragging it inside itself (or inside one of its
    /// compound clips) would create a cycle — without a limit, a stack
    /// overflow instead of a plain "not ready".
    const MAX_COMPOUND_DEPTH: u32 = 16;

    fn get_or_compute_compound_at_depth(
        &mut self,
        project: &Project,
        media_id: MediaId,
        depth: u32,
    ) -> Option<Arc<Vec<f32>>> {
        if depth >= Self::MAX_COMPOUND_DEPTH {
            return None;
        }
        let item = project.media_pool.get(media_id)?;
        let nested_id = item.compound?;
        let content_hash = item.content_hash;
        if let Some((hash, buffer)) = self.compound.get(&media_id)
            && *hash == content_hash
        {
            return Some(buffer.clone());
        }
        let nested = project.timelines.get(nested_id)?;
        let timeline_fps = nested.fps.as_f64().max(1e-9);
        let mut clips = Vec::new();
        for (_, track) in nested.audible_tracks() {
            for clip in track.clips.iter().filter(|c| !c.disabled) {
                let ClipSource::Media(inner_id) = &clip.source else {
                    continue;
                };
                let Some(inner_item) = project.media_pool.get(*inner_id) else {
                    continue;
                };
                // `get_or_request` answers `None` both for "not decided yet"
                // and "no audio on this stream" (same treatment for live
                // listening, where it makes no difference): only here, where
                // it has to decide whether to cache, do the two cases matter —
                // `in_progress` tells them apart.
                let buffer = if inner_item.compound.is_some() {
                    let Some(buffer) = self.get_or_compute_compound_at_depth(project, *inner_id, depth + 1) else {
                        return None;
                    };
                    buffer
                } else {
                    let key = (inner_item.path.clone(), clip.audio_stream_index);
                    match self.get_or_request(&inner_item.path, clip.audio_stream_index) {
                        Some(buffer) => buffer,
                        None if self.in_progress.contains(&key) => return None,
                        None => continue,
                    }
                };
                let clip_fps = inner_item.meta.fps.as_f64().max(1e-9);
                if let Some(mix_clip) =
                    vv_audio::mixer::mix_clip_from(clip, timeline_fps, clip_fps, self.sample_rate, self.channels as u64, buffer)
                {
                    clips.push(mix_clip);
                }
            }
        }
        let snapshot = vv_audio::mixer::MixSnapshot {
            sample_rate: self.sample_rate,
            channels: self.channels,
            clips,
        };
        let len = snapshot.clips.iter().map(|c| c.start + c.len).max().unwrap_or(0);
        let mut buffer = vec![0.0f32; len as usize * self.channels.max(1) as usize];
        vv_audio::mixer::mix_range(&snapshot, 0, &mut buffer);
        let buffer = Arc::new(buffer);
        self.compound.insert(media_id, (content_hash, buffer.clone()));
        Some(buffer)
    }

    /// Collects the ready buffers (partial ones too); `true` if at least one
    /// arrived (the mixer snapshot must be rebuilt).
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

/// `false` if the receiver is gone (the worker must exit).
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
    fn get_or_compute_compound_waits_for_its_real_media_then_caches_the_mixdown() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compound_source.wav");
        vv_media::test_support::ffmpeg(
            &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1"],
            &path,
        );

        let mut project = Project::default();
        let real_media = project.media_pool.insert(vv_core::MediaItem {
            path: path.clone(),
            meta: vv_core::MediaMeta {
                duration_frames: 25,
                fps: vv_core::Rational::new(25, 1),
                width: 0,
                height: 0,
                has_video: false,
                has_audio: true,
                sample_rate: 48_000,
                channels: 1,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: None,
        });
        let real_clip = vv_core::Clip::from_source_range(
            vv_core::ClipId(1),
            vv_core::ClipSource::Media(real_media),
            0,
            25,
            0,
            vv_core::Rational::one(),
        );
        let nested_id = project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1, 1),
            tracks: vec![vv_core::Track {
                kind: vv_core::TrackKind::Audio,
                clips: vec![real_clip],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            }],
        });
        let compound_media = project.media_pool.insert(vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 25,
                fps: vv_core::Rational::new(25, 1),
                width: 0,
                height: 0,
                has_video: false,
                has_audio: true,
                sample_rate: 48_000,
                channels: 1,
                audio_streams: 1,
            },
            content_hash: 2,
            compound: Some(nested_id),
        });

        let mut cache = MixBufferCache::spawn(48_000, 1);
        assert!(
            cache.get_or_compute_compound(&project, compound_media).is_none(),
            "il media vero non è ancora decodificato: la compound clip non è pronta"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let buffer = loop {
            assert!(std::time::Instant::now() < deadline, "il mixdown non è mai diventato pronto");
            cache.poll();
            if let Some(buffer) = cache.get_or_compute_compound(&project, compound_media) {
                break buffer;
            }
            std::thread::yield_now();
        };
        assert!(buffer.iter().any(|&s| s.abs() > 0.01), "il sine wave deve arrivare nel mixdown");

        // Same content_hash: the second call returns the cached buffer,
        // it does not recompute a new one.
        let cached = cache.get_or_compute_compound(&project, compound_media).unwrap();
        assert!(Arc::ptr_eq(&buffer, &cached));
    }

    #[test]
    fn buffers_are_decoded_in_background_per_path_and_stream() {
        let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two_streams.mkv");
        // Stream 0 mono 44.1 kHz of 1s, stream 1 stereo 48 kHz of 0.5s.
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
