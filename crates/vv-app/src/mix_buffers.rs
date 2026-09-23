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
        eprintln!("[mix_buffers] decode failed for {}: {e}", path.display());
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
#[path = "tests/mix_buffers.rs"]
mod tests;
