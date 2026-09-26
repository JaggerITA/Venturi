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
    /// Frames of `buffer` per timeline frame: the clip speed (varispeed),
    /// 1 when `buffer` is already stretched.
    pub step: f64,
    /// Interleaved at the snapshot's `sample_rate`/`channels`.
    pub buffer: Arc<Vec<f32>>,
    pub gain_db: Keyframed<f32>,
    pub clip_fps: f64,
    /// Audio frame of the media at `start` and media frames per timeline
    /// frame: where the gain keyframes are read, even from a stretched copy.
    pub media_offset: u64,
    pub media_step: f64,
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

    /// Clips whose audio is `Pending` are silent.
    pub fn from_timeline(
        project: &Project,
        timeline: &Timeline,
        sample_rate: u32,
        channels: u16,
        source: &mut impl AudioSource,
    ) -> Self {
        collect_clips(project, timeline, sample_rate, channels, source, 0).0
    }
}

/// A clip's audio, as an `AudioSource` answers it.
pub enum ClipAudio {
    /// Already at the snapshot's `sample_rate`/`channels`.
    Ready(Arc<Vec<f32>>),
    /// Like `Ready`, but only the start of a buffer still decoding: it plays,
    /// yet a compound containing it is not cached.
    Partial(Arc<Vec<f32>>),
    /// Still decoding, nothing yet: silent for now, and so is a compound
    /// containing it.
    Pending,
    /// No audio there (e.g. a stream index past the file's streams).
    Missing,
}

/// The decoded audio a mix is built from. Compound clips never reach `file`:
/// their mixdown is built from the nested timeline (`compound_mixdown`).
pub trait AudioSource {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio;

    /// A mixdown kept by `store_compound`, if still valid for `content_hash`.
    fn cached_compound(&mut self, _media_id: MediaId, _content_hash: u64) -> Option<Arc<Vec<f32>>> {
        None
    }

    fn store_compound(&mut self, _media_id: MediaId, _content_hash: u64, _mixdown: Arc<Vec<f32>>) {}

    /// Frames `range` of `buffer` time-stretched by `tempo`, pitch
    /// preserved. Synchronous here; the preview answers `Pending` while it
    /// works in the background.
    fn stretched(
        &mut self,
        buffer: &Arc<Vec<f32>>,
        range: std::ops::Range<u64>,
        tempo: f64,
        sample_rate: u32,
        channels: u16,
    ) -> ClipAudio {
        stretch_range(buffer, range, tempo, sample_rate, channels)
            .map_or(ClipAudio::Missing, |b| ClipAudio::Ready(Arc::new(b)))
    }
}

pub fn stretch_range(
    buffer: &[f32],
    range: std::ops::Range<u64>,
    tempo: f64,
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    let ch = channels.max(1) as usize;
    let end = (range.end as usize * ch).min(buffer.len());
    let start = (range.start as usize * ch).min(end);
    crate::stretch_samples(&buffer[start..end], sample_rate, channels, tempo)
}

/// `None` is `Missing`; no compound cache.
impl<F: FnMut(&Path, usize) -> Option<Arc<Vec<f32>>>> AudioSource for F {
    fn file(&mut self, path: &Path, stream: usize) -> ClipAudio {
        self(path, stream).map_or(ClipAudio::Missing, ClipAudio::Ready)
    }
}

/// How much of a timeline's audio was ready.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Readiness {
    Complete,
    Partial,
    Pending,
}

/// The mix clips of `timeline`, and the least ready of them.
fn collect_clips(
    project: &Project,
    timeline: &Timeline,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
    depth: u32,
) -> (MixSnapshot, Readiness) {
    let fps = timeline.fps.as_f64().max(1e-9);
    let ch = channels.max(1) as u64;
    let mut clips = Vec::new();
    let mut readiness = Readiness::Complete;
    for (_, track) in timeline.audible_tracks() {
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let Some(item) = project.media_pool.get(*media_id) else {
                continue;
            };
            let audio = if item.compound.is_some() {
                compound_audio(project, *media_id, sample_rate, channels, source, depth)
            } else {
                source.file(&item.path, clip.audio_stream_index)
            };
            let pitch_corrected = clip.pitch_correction && !clip.speed.is_one();
            let buffer = match audio {
                ClipAudio::Ready(buffer) => buffer,
                // Stretching half a buffer would have to be redone anyway.
                ClipAudio::Partial(_) if pitch_corrected => {
                    readiness = Readiness::Pending;
                    continue;
                }
                ClipAudio::Partial(buffer) => {
                    readiness = readiness.max(Readiness::Partial);
                    buffer
                }
                ClipAudio::Pending => {
                    readiness = Readiness::Pending;
                    continue;
                }
                ClipAudio::Missing => continue,
            };
            let clip_fps = item.meta.fps.as_f64().max(1e-9);
            let Some(mut mix_clip) = mix_clip_from(clip, fps, clip_fps, sample_rate, ch, buffer)
            else {
                continue;
            };
            if pitch_corrected {
                let range = mix_clip.source_offset
                    ..mix_clip.source_offset + (mix_clip.len as f64 * mix_clip.step).ceil() as u64;
                match source.stretched(
                    &mix_clip.buffer,
                    range,
                    mix_clip.step,
                    sample_rate,
                    channels,
                ) {
                    ClipAudio::Ready(stretched) => {
                        mix_clip.len = mix_clip.len.min(stretched.len() as u64 / ch);
                        mix_clip.buffer = stretched;
                        mix_clip.source_offset = 0;
                        mix_clip.step = 1.0;
                    }
                    ClipAudio::Pending => {
                        readiness = Readiness::Pending;
                        continue;
                    }
                    ClipAudio::Partial(_) | ClipAudio::Missing => continue,
                }
            }
            clips.push(mix_clip);
        }
    }
    let snapshot = MixSnapshot {
        sample_rate,
        channels,
        clips,
    };
    (snapshot, readiness)
}

/// The mixdown of the nested timeline of compound clip `media_id`, at
/// `sample_rate`/`channels`. `Pending` while a clip in it (at any depth) has
/// nothing yet, `Partial` while one is still decoding: only a complete one is
/// cached, or the missing piece would stay missing until `content_hash`
/// changes again.
pub fn compound_mixdown(
    project: &Project,
    media_id: MediaId,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
) -> ClipAudio {
    compound_audio(project, media_id, sample_rate, channels, source, 0)
}

fn compound_audio(
    project: &Project,
    media_id: MediaId,
    sample_rate: u32,
    channels: u16,
    source: &mut impl AudioSource,
    depth: u32,
) -> ClipAudio {
    // Past the limit (only a cycle gets here) it never becomes ready: the
    // whole cycle stays silent instead of layering itself 16 times.
    if depth >= vv_core::MAX_COMPOUND_DEPTH {
        return ClipAudio::Pending;
    }
    let Some(item) = project.media_pool.get(media_id) else {
        return ClipAudio::Missing;
    };
    let Some(nested) = item.compound.and_then(|id| project.timelines.get(id)) else {
        return ClipAudio::Missing;
    };
    if let Some(mixdown) = source.cached_compound(media_id, item.content_hash) {
        return ClipAudio::Ready(mixdown);
    }
    let (snapshot, readiness) =
        collect_clips(project, nested, sample_rate, channels, source, depth + 1);
    if readiness == Readiness::Pending {
        return ClipAudio::Pending;
    }
    let len = snapshot
        .clips
        .iter()
        .map(|c| c.start + c.len)
        .max()
        .unwrap_or(0);
    let mut mixdown = vec![0.0f32; len as usize * channels.max(1) as usize];
    mix_range(&snapshot, 0, &mut mixdown);
    let mixdown = Arc::new(mixdown);
    if readiness == Readiness::Partial {
        return ClipAudio::Partial(mixdown);
    }
    source.store_compound(media_id, item.content_hash, mixdown.clone());
    ClipAudio::Ready(mixdown)
}

/// `None` if the buffer does not cover even one sample of the clip.
fn mix_clip_from(
    clip: &vv_core::Clip,
    timeline_fps: f64,
    clip_fps: f64,
    sample_rate: u32,
    channels: u64,
    buffer: Arc<Vec<f32>>,
) -> Option<MixClip> {
    let ch = channels.max(1);
    let buffer_frames = buffer.len() as u64 / ch;
    let source_offset = seconds_to_frames(
        clip.media_secs_at(clip.timeline_start, timeline_fps),
        sample_rate,
    )
    .min(buffer_frames);
    let step = clip.speed.as_f64();
    // The last frame read is interpolated with the one after it.
    let readable = ((buffer_frames - source_offset) as f64 / step).floor() as u64;
    let len = seconds_to_frames(clip.timeline_len as f64 / timeline_fps, sample_rate)
        .min(readable.saturating_sub(u64::from(step != 1.0)));
    if len == 0 {
        return None;
    }
    let fade_in = seconds_to_frames(clip.fade_in as f64 / timeline_fps, sample_rate).min(len);
    let fade_out = seconds_to_frames(clip.fade_out as f64 / timeline_fps, sample_rate).min(len);
    Some(MixClip {
        start: timeline_frame_to_sample(clip.timeline_start, timeline_fps, sample_rate),
        len,
        source_offset,
        step,
        buffer,
        gain_db: clip.effects.gain_db.clone(),
        clip_fps,
        media_offset: source_offset,
        media_step: step,
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

            let dst = ((f - start) as usize) * ch;
            if clip.step == 1.0 {
                let src = ((clip.source_offset + in_clip) as usize) * ch;
                let count =
                    ((block_end - f) as usize * ch).min(clip.buffer.len().saturating_sub(src));
                for (d, s) in out[dst..dst + count]
                    .iter_mut()
                    .zip(&clip.buffer[src..src + count])
                {
                    *d += s * gain;
                }
            } else {
                let frames = &mut out[dst..dst + (block_end - f) as usize * ch];
                for (i, frame) in frames.chunks_exact_mut(ch).enumerate() {
                    let pos = clip.source_offset as f64 + (in_clip + i as u64) as f64 * clip.step;
                    add_resampled(&clip.buffer, ch, pos, clip.step, gain, frame);
                }
            }
            f = block_end;
        }
    }
}

/// Varispeed read of the frame at fractional position `pos`: linear
/// interpolation slowing down; speeding up, the average of the `step`
/// frames skipped over, a crude low-pass against aliasing.
fn add_resampled(buffer: &[f32], ch: usize, pos: f64, step: f64, gain: f32, out: &mut [f32]) {
    let frames = buffer.len() / ch;
    let at = |frame: usize, c: usize| buffer[frame.min(frames - 1) * ch + c];
    let i = pos as usize;
    if step < 1.0 {
        let t = (pos - i as f64) as f32;
        for (c, o) in out.iter_mut().enumerate() {
            *o += (at(i, c) + (at(i + 1, c) - at(i, c)) * t) * gain;
        }
    } else {
        let n = (step as usize).max(1);
        for (c, o) in out.iter_mut().enumerate() {
            let sum: f32 = (i..i + n).map(|frame| at(frame, c)).sum();
            *o += sum / n as f32 * gain;
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
    let media_frame =
        clip.media_offset as f64 + (block * GAIN_BLOCK_FRAMES) as f64 * clip.media_step;
    let media_secs = media_frame / sample_rate as f64;
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
