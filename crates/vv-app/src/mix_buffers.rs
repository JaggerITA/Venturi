//! Audio buffers already in the mixer's format, per `(path, audio_stream_index)`.
//! Decode + resample on a dedicated thread. The buffer is published
//! partially while it grows: the mixer plays the start of the track before
//! the decode finishes, past the ready part there is silence.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use vv_audio::{AudioSource, ClipAudio};
use vv_core::MediaId;

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
    /// Mixdowns of compound clips, with the `content_hash` they were
    /// computed with (see `vv_audio::mixer::compound_mixdown`).
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

/// Every buffer is at the `sample_rate`/`channels` given to `spawn`.
impl AudioSource for MixBufferCache {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        let buffer = self.get_or_request(path, stream);
        let decoding = self.in_progress.contains(&(path.to_path_buf(), stream));
        match (buffer, decoding) {
            (Some(buffer), false) => ClipAudio::Ready(buffer),
            (Some(buffer), true) => ClipAudio::Partial(buffer),
            (None, true) => ClipAudio::Pending,
            (None, false) => ClipAudio::Missing,
        }
    }

    fn cached_compound(&mut self, media_id: MediaId, content_hash: u64) -> Option<Arc<Vec<f32>>> {
        self.compound
            .get(&media_id)
            .filter(|(hash, _)| *hash == content_hash)
            .map(|(_, mixdown)| mixdown.clone())
    }

    fn store_compound(&mut self, media_id: MediaId, content_hash: u64, mixdown: Arc<Vec<f32>>) {
        self.compound.insert(media_id, (content_hash, mixdown));
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
