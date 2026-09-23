//! Continuous mixer of the timeline audio tracks: an immutable snapshot
//! of the clips (built by the UI thread) + `mix_range`, a pure function used
//! both by the cpal callback and by the export, so preview and export
//! produce the same mix.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vv_core::{ClipSource, FrameIdx, Keyframed, MediaId, Project, Timeline};

pub const PROJECT_SAMPLE_RATE: u32 = 48_000;
/// Control-rate (~60Hz) granularity of the keyframed gain, not sample-accurate.
pub const GAIN_BLOCK_FRAMES: u64 = 800;


pub struct MixClip {
    /// In timeline audio frames (one sample per channel).
    pub start: u64,
    pub len: u64,
    /// Audio frame of `buffer` corresponding to `start`.
    pub source_offset: u64,
    /// Interleaved at the snapshot's `sample_rate`/`channels`.
    pub buffer: Arc<Vec<f32>>,
    pub gain_db: Keyframed<f32>,
    pub clip_fps: f64,
    /// Fades, in timeline audio frames from the start/end of
    /// `len` (see `Clip::fade_multiplier_at`, same semantics on samples).
    pub fade_in: u64,
    pub fade_out: u64,
}

pub struct MixSnapshot {
    pub sample_rate: u32,
    pub channels: u16,
    pub clips: Vec<MixClip>,
}

impl MixSnapshot {
    pub fn empty(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            clips: Vec::new(),
        }
    }

    /// `buffer_for` gives the buffer already at the `sample_rate`/`channels` of a
    /// real file; `compound_buffer_for` that of the mixdown of a compound clip
    /// (see `vv_app::mix_buffers` for how it computes and caches it — it does not
    /// matter here, only that it can be asked given a `MediaId`). `None` from
    /// either one (not ready yet, or no audio) makes the clip silent.
    pub fn from_timeline(
        project: &Project,
        timeline: &Timeline,
        sample_rate: u32,
        channels: u16,
        mut buffer_for: impl FnMut(&Path, usize) -> Option<Arc<Vec<f32>>>,
        mut compound_buffer_for: impl FnMut(&Project, MediaId) -> Option<Arc<Vec<f32>>>,
    ) -> Self {
        let fps = timeline.fps.as_f64().max(1e-9);
        let ch = channels.max(1) as u64;
        let mut clips = Vec::new();
        for (_, track) in timeline.audible_tracks() {
            for clip in track.clips.iter().filter(|c| !c.disabled) {
                let ClipSource::Media(media_id) = &clip.source else {
                    continue;
                };
                let Some(item) = project.media_pool.get(*media_id) else {
                    continue;
                };
                let buffer = if item.compound.is_some() {
                    compound_buffer_for(project, *media_id)
                } else {
                    buffer_for(&item.path, clip.audio_stream_index)
                };
                let Some(buffer) = buffer else {
                    continue;
                };
                let clip_fps = item.meta.fps.as_f64().max(1e-9);
                if let Some(mix_clip) = mix_clip_from(clip, fps, clip_fps, sample_rate, ch, buffer) {
                    clips.push(mix_clip);
                }
            }
        }
        Self {
            sample_rate,
            channels,
            clips,
        }
    }
}

/// The `MixClip` of `clip`, given the buffer already decoded/composed at
/// `sample_rate`: the part of `from_timeline` independent of how `buffer` was
/// obtained (a real file or the mixdown of a compound clip), reused
/// also by `vv_app::mix_buffers::MixBufferCache::get_or_compute_compound`
/// to build the exact same mix of a nested timeline without
/// going through `from_timeline` (which would require two closures both
/// mutable on the same cache, in conflict — see there for the details).
/// `None` if the buffer does not cover even one sample of the clip.
pub fn mix_clip_from(
    clip: &vv_core::Clip,
    timeline_fps: f64,
    clip_fps: f64,
    sample_rate: u32,
    channels: u64,
    buffer: Arc<Vec<f32>>,
) -> Option<MixClip> {
    let ch = channels.max(1);
    let buffer_frames = buffer.len() as u64 / ch;
    let source_offset = seconds_to_frames(clip.media_secs_at(clip.timeline_start, timeline_fps), sample_rate)
        .min(buffer_frames);
    let len = seconds_to_frames(clip.timeline_len as f64 / timeline_fps, sample_rate)
        .min(buffer_frames - source_offset);
    if len == 0 {
        return None;
    }
    let fade_in = seconds_to_frames(clip.fade_in as f64 / timeline_fps, sample_rate).min(len);
    let fade_out = seconds_to_frames(clip.fade_out as f64 / timeline_fps, sample_rate).min(len);
    Some(MixClip {
        start: timeline_frame_to_sample(clip.timeline_start, timeline_fps, sample_rate),
        len,
        source_offset,
        buffer,
        gain_db: clip.effects.gain_db.clone(),
        clip_fps,
        fade_in,
        fade_out,
    })
}

fn seconds_to_frames(secs: f64, sample_rate: u32) -> u64 {
    (secs.max(0.0) * sample_rate as f64).round() as u64
}

pub fn timeline_frame_to_sample(frame: FrameIdx, fps: f64, sample_rate: u32) -> u64 {
    seconds_to_frames(frame as f64 / fps.max(1e-9), sample_rate)
}

pub fn sample_to_timeline_frame(sample: u64, fps: f64, sample_rate: u32) -> FrameIdx {
    (sample as f64 / sample_rate as f64 * fps).floor() as FrameIdx
}

/// Writes into `out` (interleaved, `snapshot.channels` channels) the mix
/// starting from timeline audio frame `start`. No allocations: it runs
/// in the realtime callback.
pub fn mix_range(snapshot: &MixSnapshot, start: u64, out: &mut [f32]) {
    out.fill(0.0);
    let ch = snapshot.channels as usize;
    if ch == 0 {
        return;
    }
    let end = start + (out.len() / ch) as u64;
    for clip in &snapshot.clips {
        let clip_end = clip.start + clip.len;
        let to = end.min(clip_end);
        let mut f = start.max(clip.start);
        while f < to {
            let in_clip = f - clip.start;
            let block = in_clip / GAIN_BLOCK_FRAMES;
            let block_end = (clip.start + (block + 1) * GAIN_BLOCK_FRAMES).min(to);
            let gain = block_gain_linear(clip, block, snapshot.sample_rate)
                * fade_multiplier(clip, in_clip);

            let src = ((clip.source_offset + in_clip) as usize) * ch;
            let dst = ((f - start) as usize) * ch;
            let count = ((block_end - f) as usize * ch).min(clip.buffer.len().saturating_sub(src));
            for (d, s) in out[dst..dst + count]
                .iter_mut()
                .zip(&clip.buffer[src..src + count])
            {
                *d += s * gain;
            }
            f = block_end;
        }
    }
}

/// Linear fade ramp at `in_clip` samples from the start of the
/// clip, same semantics as `Clip::fade_multiplier_at` but in audio samples.
fn fade_multiplier(clip: &MixClip, in_clip: u64) -> f32 {
    let in_ramp = if clip.fade_in > 0 {
        (in_clip as f32 / clip.fade_in as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let out_ramp = if clip.fade_out > 0 {
        let from_end = clip.len.saturating_sub(in_clip);
        (from_end as f32 / clip.fade_out as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    in_ramp * out_ramp
}

fn block_gain_linear(clip: &MixClip, block: u64, sample_rate: u32) -> f32 {
    if clip.gain_db.is_constant() {
        return db_to_linear(clip.gain_db.default);
    }
    // The gain keyframes live in *source* frames: the frame covering
    // this instant of the media.
    let media_secs = (clip.source_offset + block * GAIN_BLOCK_FRAMES) as f64 / sample_rate as f64;
    let source_frame = (media_secs * clip.clip_fps).floor() as FrameIdx;
    db_to_linear(clip.gain_db.value_at(source_frame))
}

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// From `from` to `to` channels, appending to `out`. Symmetric, not a broadcast
/// downmix: going down, channel `i` is averaged onto `i % to` (on 5.1
/// ffmpeg groups L,C,Ls and R,LFE,Rs), going up, channel `i` copies
/// `i % from`.
pub fn remix_channels_into(samples: &[f32], from: u16, to: u16, out: &mut Vec<f32>) {
    if from == 0 || to == 0 {
        return;
    }
    if from == to {
        out.extend_from_slice(samples);
        return;
    }
    let (from, to) = (from as usize, to as usize);
    out.reserve(samples.len() / from * to);
    for frame in samples.chunks_exact(from) {
        if from > to {
            for c in 0..to {
                let (sum, n) = frame
                    .iter()
                    .skip(c)
                    .step_by(to)
                    .fold((0.0, 0u32), |(sum, n), &v| (sum + v, n + 1));
                out.push(sum / n as f32);
            }
        } else {
            out.extend((0..to).map(|c| frame[c % from]));
        }
    }
}

/// Audio already stretched to `tempo` (fast forward): frame `i` of the
/// concatenation of `chunks` corresponds to timeline audio frame
/// `origin + i * tempo`.
#[derive(Clone)]
pub struct StretchedWindow {
    pub tempo: u64,
    pub origin: u64,
    pub chunks: Vec<Arc<Vec<f32>>>,
}

impl StretchedWindow {
    pub fn frames(&self, channels: u16) -> u64 {
        let ch = channels.max(1) as usize;
        self.chunks.iter().map(|c| (c.len() / ch) as u64).sum()
    }

    /// First uncovered timeline audio frame.
    pub fn covered_until(&self, channels: u16) -> u64 {
        self.origin + self.frames(channels) * self.tempo
    }
}

/// What the callback plays: the mix, or at speeds > 1x the stretched
/// window.
pub struct MixerState {
    pub mix: Arc<MixSnapshot>,
    pub stretched: Option<StretchedWindow>,
}

/// Writes into `out` the stretched audio corresponding to timeline audio
/// frame `position`; silence outside the window. No allocations.
pub fn render_stretched(window: &StretchedWindow, channels: u16, position: u64, out: &mut [f32]) {
    out.fill(0.0);
    let ch = channels as usize;
    if ch == 0 || window.tempo == 0 || position < window.origin {
        return;
    }
    let mut skip = ((position - window.origin) / window.tempo) as usize * ch;
    let mut written = 0;
    for chunk in &window.chunks {
        if skip >= chunk.len() {
            skip -= chunk.len();
            continue;
        }
        let take = (chunk.len() - skip).min(out.len() - written);
        out[written..written + take].copy_from_slice(&chunk[skip..skip + take]);
        written += take;
        skip = 0;
        if written == out.len() {
            break;
        }
    }
}

/// Single cpal stream playing the current state. The position (timeline
/// audio frame) is the playback clock.
pub struct Mixer {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    position: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<Arc<MixerState>>>>,
    /// Published states still referenced by the callback: keeping them here
    /// guarantees that the last drop (with deallocation) happens on the UI
    /// thread, never on the audio one.
    retained: Vec<Arc<MixerState>>,
    peak_left_bits: Arc<AtomicU32>,
    peak_right_bits: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
}

impl Mixer {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no audio output device")?;
        // Native channels of the device: asking for others makes PipeWire insert a
        // remix that adds output latency, visible as audio lagging
        // behind playhead and waveform.
        let channels = device
            .default_output_config()
            .map(|c| c.channels())
            .unwrap_or(2);
        let sample_rate = PROJECT_SAMPLE_RATE;
        let config = cpal::StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let playing = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(0));
        let initial = Arc::new(MixerState {
            mix: Arc::new(MixSnapshot::empty(sample_rate, channels)),
            stretched: None,
        });
        let pending = Arc::new(Mutex::new(None));
        let peak_left_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let peak_right_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));

        let cb_playing = playing.clone();
        let cb_position = position.clone();
        let cb_pending = pending.clone();
        let cb_peak_left = peak_left_bits.clone();
        let cb_peak_right = peak_right_bits.clone();
        let mut current = initial.clone();
        let ch = channels.max(1) as usize;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if let Ok(mut slot) = cb_pending.try_lock()
                        && let Some(next) = slot.take()
                    {
                        current = next;
                    }
                    if cb_playing.load(Ordering::Relaxed) && current.mix.channels as usize == ch {
                        let pos = cb_position.load(Ordering::Relaxed);
                        let frames = (data.len() / ch) as u64;
                        let advance = match &current.stretched {
                            Some(window) => {
                                render_stretched(window, channels, pos, data);
                                frames * window.tempo
                            }
                            None => {
                                mix_range(&current.mix, pos, data);
                                frames
                            }
                        };
                        // A seek that arrived during the mix wins over the advance.
                        let _ = cb_position.compare_exchange(
                            pos,
                            pos + advance,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    } else {
                        data.fill(0.0);
                    }
                    let (l, r) = stereo_peak(data, ch);
                    cb_peak_left.store(l.to_bits(), Ordering::Relaxed);
                    cb_peak_right.store(r.to_bits(), Ordering::Relaxed);
                },
                move |err| eprintln!("vv-audio: mixer stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        Ok(Self {
            _stream: stream,
            playing,
            position,
            pending,
            retained: vec![initial],
            peak_left_bits,
            peak_right_bits,
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn set_state(&mut self, state: Arc<MixerState>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
        self.retained.retain(|s| Arc::strong_count(s) > 1);
        self.retained.push(state);
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek(&self, timeline_sample: u64) {
        self.position.store(timeline_sample, Ordering::Relaxed);
    }

    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    pub fn peak_linear_stereo(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak_left_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.peak_right_bits.load(Ordering::Relaxed)),
        )
    }
}

fn stereo_peak(data: &[f32], channels: usize) -> (f32, f32) {
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    for frame in data.chunks(channels.max(1)) {
        if let Some(&s) = frame.first() {
            left = left.max(s.abs());
        }
        match frame.get(1) {
            Some(&s) => right = right.max(s.abs()),
            None => right = left,
        }
    }
    (left, right)
}

#[cfg(test)]
#[path = "tests/mixer.rs"]
mod tests;
