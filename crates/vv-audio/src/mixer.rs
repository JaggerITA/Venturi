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
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vv_core::{
        Clip, ClipId, Interpolation, MediaItem, MediaMeta, Rational, Track, TrackKind,
    };

    const RATE: u32 = 100;

    fn clip_at(media: vv_core::MediaId, start: FrameIdx, source_in: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(
            ClipId(0),
            ClipSource::Media(media),
            source_in,
            source_in + len,
            start,
            Rational::one(),
        )
    }

    /// Project at 10 fps with two media (`a.wav`, `b.wav`): at `RATE` = 100
    /// every timeline frame is 10 audio frames.
    fn project() -> (Project, vv_core::MediaId, vv_core::MediaId) {
        let mut project = Project::default();
        let meta = MediaMeta {
            duration_frames: 100,
            fps: Rational::new(10, 1),
            width: 0,
            height: 0,
            has_video: true,
            has_audio: true,
            sample_rate: RATE,
            channels: 1,
            audio_streams: 1,
        };
        let a = project.media_pool.insert(MediaItem {
            path: PathBuf::from("a.wav"),
            meta: meta.clone(),
            content_hash: 1,
            compound: None,
        });
        let b = project.media_pool.insert(MediaItem {
            path: PathBuf::from("b.wav"),
            meta,
            content_hash: 2,
            compound: None,
        });
        (project, a, b)
    }

    fn timeline(tracks: Vec<Track>) -> Timeline {
        Timeline {
            name: "t".into(),
            fps: Rational::new(10, 1),
            resolution: (4, 2),
            tracks,
        }
    }

    fn audio_track(clips: Vec<Clip>) -> Track {
        Track {
            kind: TrackKind::Audio,
            clips,
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }
    }

    /// Mono: `a` is always 0.5, `b` is a ramp (sample i = i/1000);
    /// stream 1 of `a` is 0.25.
    fn buffers(path: &Path, stream: usize) -> Option<Arc<Vec<f32>>> {
        match (path.to_str()?, stream) {
            ("a.wav", 0) => Some(Arc::new(vec![0.5; 1000])),
            ("a.wav", 1) => Some(Arc::new(vec![0.25; 1000])),
            ("b.wav", 0) => Some(Arc::new((0..1000).map(|i| i as f32 / 1000.0).collect())),
            _ => None,
        }
    }

    fn render(project: &Project, tl: &Timeline, start: u64, frames: usize) -> Vec<f32> {
        let snap = MixSnapshot::from_timeline(project, tl, RATE, 1, buffers, |_, _| None);
        let mut out = vec![9.0; frames];
        mix_range(&snap, start, &mut out);
        out
    }

    #[test]
    fn gap_is_silence() {
        let (project, a, _) = project();
        let tl = timeline(vec![audio_track(vec![clip_at(a, 5, 0, 2)])]);
        let out = render(&project, &tl, 0, 50);
        assert!(out.iter().all(|&s| s == 0.0));
        let empty = timeline(vec![]);
        assert!(render(&project, &empty, 0, 10).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn offset_clip_plays_its_source_range_at_its_timeline_position() {
        let (project, _, b) = project();
        // Timeline frame 2 (= audio 20) plays the source from frame 3 (= audio 30).
        let tl = timeline(vec![audio_track(vec![clip_at(b, 2, 3, 1)])]);
        let out = render(&project, &tl, 15, 20);
        assert!(out[..5].iter().all(|&s| s == 0.0));
        for i in 0..10 {
            assert_eq!(out[5 + i], (30 + i) as f32 / 1000.0);
        }
        assert!(out[15..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn overlapping_clips_on_different_tracks_are_summed() {
        let (project, a, b) = project();
        let tl = timeline(vec![
            audio_track(vec![clip_at(a, 0, 0, 5)]),
            audio_track(vec![clip_at(b, 0, 0, 5)]),
        ]);
        let out = render(&project, &tl, 10, 5);
        for (i, s) in out.iter().enumerate() {
            assert_eq!(*s, 0.5 + (10 + i) as f32 / 1000.0);
        }
    }

    #[test]
    fn muted_track_is_excluded() {
        let (project, a, b) = project();
        let mut muted = audio_track(vec![clip_at(a, 0, 0, 5)]);
        muted.muted = true;
        let tl = timeline(vec![muted, audio_track(vec![clip_at(b, 0, 0, 5)])]);
        let out = render(&project, &tl, 0, 5);
        for (i, s) in out.iter().enumerate() {
            assert_eq!(*s, i as f32 / 1000.0);
        }
    }

    #[test]
    fn only_solo_tracks_play_when_any_is_solo() {
        let (project, a, b) = project();
        let mut solo = audio_track(vec![clip_at(b, 0, 0, 5)]);
        solo.solo = true;
        let tl = timeline(vec![audio_track(vec![clip_at(a, 0, 0, 5)]), solo]);
        let out = render(&project, &tl, 0, 5);
        for (i, s) in out.iter().enumerate() {
            assert_eq!(*s, i as f32 / 1000.0);
        }
    }

    #[test]
    fn disabled_clip_is_excluded() {
        let (project, a, b) = project();
        let mut disabled = clip_at(a, 0, 0, 5);
        disabled.disabled = true;
        let tl = timeline(vec![
            audio_track(vec![disabled]),
            audio_track(vec![clip_at(b, 0, 0, 5)]),
        ]);
        let out = render(&project, &tl, 0, 5);
        for (i, s) in out.iter().enumerate() {
            assert_eq!(*s, i as f32 / 1000.0);
        }
    }

    #[test]
    fn video_tracks_are_ignored() {
        let (project, a, _) = project();
        let mut video = audio_track(vec![clip_at(a, 0, 0, 5)]);
        video.kind = TrackKind::Video;
        let tl = timeline(vec![video]);
        assert!(render(&project, &tl, 0, 20).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_compound_clip_is_routed_to_compound_buffer_for_never_to_buffer_for() {
        let mut project = Project::default();
        let nested = project.timelines.insert(Timeline {
            name: "n".into(),
            fps: Rational::new(10, 1),
            resolution: (1, 1),
            tracks: vec![],
        });
        let compound_media = project.media_pool.insert(MediaItem {
            path: PathBuf::from("Compound Clip 1"),
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(10, 1),
                width: 0,
                height: 0,
                has_video: false,
                has_audio: true,
                sample_rate: RATE,
                channels: 1,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: Some(nested),
        });
        let tl = timeline(vec![audio_track(vec![clip_at(compound_media, 0, 0, 5)])]);

        let mut buffer_for_called = false;
        let snap = MixSnapshot::from_timeline(
            &project,
            &tl,
            RATE,
            1,
            |_, _| {
                buffer_for_called = true;
                None
            },
            |_, id| (id == compound_media).then(|| Arc::new(vec![0.5; 50])),
        );
        assert!(!buffer_for_called, "a compound clip must never go through buffer_for");
        assert_eq!(snap.clips.len(), 1, "compound_buffer_for answered: the clip enters the mix");
    }

    /// The bug case: media at 9.99 fps (10000/1001, the small-scale analogue
    /// of 59.94 on 60) on a 10 fps timeline. Conformed, the clip
    /// lasts on the timeline as long as its audio does, and the mix at the end of the clip is
    /// still aligned to the right sample instead of being cut off.
    #[test]
    fn a_conformed_clip_lasts_as_long_as_its_audio_and_does_not_drift() {
        const SOURCE_FRAMES: FrameIdx = 1000;
        const AUDIO_SAMPLES: usize = 10_010; // 1000 frames / 9.99 fps = 100.1 s

        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: PathBuf::from("slow.wav"),
            meta: MediaMeta {
                duration_frames: SOURCE_FRAMES,
                fps: Rational::new(10_000, 1001),
                width: 0,
                height: 0,
                has_video: true,
                has_audio: true,
                sample_rate: RATE,
                channels: 1,
                audio_streams: 1,
            },
            content_hash: 7,
            compound: None,
        });
        let rate = Rational::conform_rate(Rational::new(10, 1), Rational::new(10_000, 1001));
        let clip =
            Clip::from_source_range(ClipId(0), ClipSource::Media(media), 0, SOURCE_FRAMES, 0, rate);
        assert_eq!(clip.timeline_len, 1001, "100,1 s a 10 fps");
        let tl = timeline(vec![audio_track(vec![clip])]);

        let buffer: Arc<Vec<f32>> =
            Arc::new((0..AUDIO_SAMPLES).map(|i| i as f32 / 100_000.0).collect());
        let buffer_for = |path: &Path, _stream: usize| {
            (path.to_str() == Some("slow.wav")).then(|| buffer.clone())
        };
        let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, buffer_for, |_, _| None);
        assert_eq!(snap.clips.len(), 1);
        assert_eq!(
            snap.clips[0].len, AUDIO_SAMPLES as u64,
            "all of the media audio fits in the clip, nothing cut"
        );

        // Last 10 samples of the clip: still the ones at the end of the buffer,
        // no drift accumulated over the preceding 100 s.
        let mut out = vec![9.0; 10];
        mix_range(&snap, AUDIO_SAMPLES as u64 - 10, &mut out);
        for (i, s) in out.iter().enumerate() {
            let expected = (AUDIO_SAMPLES - 10 + i) as f32 / 100_000.0;
            assert!((s - expected).abs() < 1e-6, "campione {i}: {s} != {expected}");
        }
    }

    /// A split in the middle of a source frame does not move the audio: the right
    /// half restarts from the sample the original was playing at that point.
    #[test]
    fn splitting_a_conformed_clip_mid_source_frame_keeps_every_sample() {
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: PathBuf::from("slow.wav"),
            meta: MediaMeta {
                duration_frames: 1000,
                fps: Rational::new(10_000, 1001),
                width: 0,
                height: 0,
                has_video: true,
                has_audio: true,
                sample_rate: RATE,
                channels: 1,
                audio_streams: 1,
            },
            content_hash: 7,
            compound: None,
        });
        let rate = Rational::conform_rate(Rational::new(10, 1), Rational::new(10_000, 1001));
        let clip = Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 1000, 0, rate);
        assert_eq!(clip.source_frame_at(500), clip.source_frame_at(499), "500 is mid-frame");
        let timeline_id = project.timelines.insert(timeline(vec![audio_track(vec![clip])]));

        let buffer: Arc<Vec<f32>> = Arc::new((0..10_010).map(|i| i as f32 / 100_000.0).collect());
        let buffer_for = |path: &Path, _stream: usize| {
            (path.to_str() == Some("slow.wav")).then(|| buffer.clone())
        };
        let mix_all = |project: &Project| {
            let timeline = &project.timelines[timeline_id];
            let snap = MixSnapshot::from_timeline(project, timeline, RATE, 1, buffer_for, |_, _| None);
            let mut out = vec![0.0; 10_010];
            mix_range(&snap, 0, &mut out);
            out
        };
        let before = mix_all(&project);

        let mut split = vv_core::SplitClip::new(timeline_id, 0, ClipId(1), 500);
        vv_core::Command::apply(&mut split, &mut project);
        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);

        assert_eq!(mix_all(&project), before);
    }

    #[test]
    fn constant_gain_scales_the_clip() {
        let (project, a, _) = project();
        let mut clip = clip_at(a, 0, 0, 5);
        clip.effects.gain_db = Keyframed::constant(-6.0206);
        let tl = timeline(vec![audio_track(vec![clip])]);
        for s in render(&project, &tl, 0, 10) {
            assert!((s - 0.25).abs() < 1e-3, "s={s}");
        }
    }

    #[test]
    fn keyframed_gain_is_evaluated_per_block_at_the_source_frame() {
        let (project, a, _) = project();
        // Long enough to cover several gain blocks.
        let blocks = 3;
        let len_frames = (GAIN_BLOCK_FRAMES * blocks) as usize;
        let mut clip = clip_at(a, 0, 0, (len_frames / 10) as FrameIdx);
        let mut gain = Keyframed::constant(0.0f32);
        gain.upsert(0, 0.0, Interpolation::Hold);
        // From the second block (audio 800 = source frame 80) on: practically -inf.
        gain.upsert(80, -200.0, Interpolation::Hold);
        clip.effects.gain_db = gain;
        let tl = timeline(vec![audio_track(vec![clip])]);
        let snap = MixSnapshot::from_timeline(
            &project,
            &tl,
            RATE,
            1,
            |_, _| Some(Arc::new(vec![0.5; len_frames])),
            |_, _| None,
        );
        let mut out = vec![0.0; len_frames];
        mix_range(&snap, 0, &mut out);
        let block = GAIN_BLOCK_FRAMES as usize;
        assert!(out[..block].iter().all(|&s| s == 0.5));
        assert!(out[block..].iter().all(|&s| s.abs() < 1e-6));
    }

    #[test]
    fn audio_stream_index_selects_the_buffer() {
        let (project, a, _) = project();
        let mut clip = clip_at(a, 0, 0, 5);
        clip.audio_stream_index = 1;
        let tl = timeline(vec![audio_track(vec![clip])]);
        assert!(render(&project, &tl, 0, 10).iter().all(|&s| s == 0.25));
    }

    #[test]
    fn clip_whose_buffer_is_not_ready_is_silent() {
        let (project, a, b) = project();
        let tl = timeline(vec![
            audio_track(vec![clip_at(a, 0, 0, 5)]),
            audio_track(vec![clip_at(b, 0, 0, 5)]),
        ]);
        let snap = MixSnapshot::from_timeline(
            &project,
            &tl,
            RATE,
            1,
            |p, s| (p != Path::new("b.wav")).then(|| buffers(p, s)).flatten(),
            |_, _| None,
        );
        let mut out = vec![0.0; 10];
        mix_range(&snap, 0, &mut out);
        assert!(out.iter().all(|&s| s == 0.5));
    }

    #[test]
    fn clip_longer_than_its_buffer_is_clamped() {
        let (project, a, _) = project();
        let tl = timeline(vec![audio_track(vec![clip_at(a, 0, 95, 20)])]);
        let out = render(&project, &tl, 0, 100);
        assert!(out[..50].iter().all(|&s| s == 0.5));
        assert!(out[50..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mix_is_independent_of_how_the_range_is_split() {
        let (project, a, b) = project();
        let mut ca = clip_at(a, 1, 0, 30);
        let mut kf = Keyframed::constant(0.0f32);
        kf.upsert(0, 0.0, Interpolation::Linear);
        kf.upsert(30, -12.0, Interpolation::Linear);
        ca.effects.gain_db = kf;
        let tl = timeline(vec![
            audio_track(vec![ca]),
            audio_track(vec![clip_at(b, 7, 2, 20)]),
        ]);
        let whole = render(&project, &tl, 0, 400);
        let snap = MixSnapshot::from_timeline(&project, &tl, RATE, 1, buffers, |_, _| None);
        let mut chunked = vec![0.0; 400];
        for (i, chunk) in chunked.chunks_mut(37).enumerate() {
            mix_range(&snap, (i * 37) as u64, chunk);
        }
        assert_eq!(whole, chunked);
    }

    #[test]
    fn stereo_mix_keeps_channels_interleaved() {
        let (project, a, _) = project();
        let tl = timeline(vec![audio_track(vec![clip_at(a, 1, 0, 1)])]);
        let snap = MixSnapshot::from_timeline(
            &project,
            &tl,
            RATE,
            2,
            |_, _| Some(Arc::new([0.1, 0.9].repeat(100))),
            |_, _| None,
        );
        let mut out = vec![0.0; 40];
        mix_range(&snap, 5, &mut out);
        assert!(out[..10].iter().all(|&s| s == 0.0));
        for f in out[10..30].chunks(2) {
            assert_eq!(f, [0.1, 0.9]);
        }
        assert!(out[30..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mixer_seek_and_snapshot_swaps_do_not_reopen_or_leak() {
        let mut mixer = Mixer::new().unwrap();
        mixer.seek(12_345);
        assert_eq!(mixer.position(), 12_345, "paused, the position does not advance");
        for _ in 0..50 {
            let mix = Arc::new(MixSnapshot::empty(mixer.sample_rate(), mixer.channels()));
            mixer.set_state(Arc::new(MixerState {
                mix,
                stretched: None,
            }));
        }
        assert!(mixer.retained.len() <= 3, "retained={}", mixer.retained.len());
    }

    fn window(tempo: u64, origin: u64, chunks: &[&[f32]]) -> StretchedWindow {
        StretchedWindow {
            tempo,
            origin,
            chunks: chunks.iter().map(|c| Arc::new(c.to_vec())).collect(),
        }
    }

    #[test]
    fn stretched_window_maps_timeline_position_to_stretched_frames() {
        let w = window(4, 100, &[&[1.0, 2.0, 3.0], &[4.0, 5.0]]);
        assert_eq!(w.covered_until(1), 120);

        let mut out = [9.0; 3];
        render_stretched(&w, 1, 100, &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0]);

        // Position 110 = stretched frame 2, across the chunk boundary.
        render_stretched(&w, 1, 110, &mut out);
        assert_eq!(out, [3.0, 4.0, 5.0]);

        render_stretched(&w, 1, 116, &mut out);
        assert_eq!(out, [5.0, 0.0, 0.0], "past the end: silence");
        render_stretched(&w, 1, 50, &mut out);
        assert_eq!(out, [0.0; 3], "before the origin: silence");
    }

    #[test]
    fn stretched_window_keeps_stereo_frames_aligned() {
        let w = window(2, 0, &[&[0.1, 0.2, 0.3, 0.4], &[0.5, 0.6]]);
        assert_eq!(w.covered_until(2), 6);
        let mut out = [0.0; 4];
        render_stretched(&w, 2, 2, &mut out);
        assert_eq!(out, [0.3, 0.4, 0.5, 0.6]);
    }

    #[test]
    fn sample_and_frame_conversions_round_trip() {
        let fps = 25.0;
        for frame in [0, 1, 24, 25, 1234] {
            let s = timeline_frame_to_sample(frame, fps, PROJECT_SAMPLE_RATE);
            assert_eq!(sample_to_timeline_frame(s, fps, PROJECT_SAMPLE_RATE), frame);
        }
    }

    fn downmix_interleaved(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
        let mut out = Vec::new();
        remix_channels_into(samples, from, to, &mut out);
        out
    }

    #[test]
    fn downmix_interleaved_is_a_noop_when_channel_counts_match() {
        let samples = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(downmix_interleaved(&samples, 2, 2), samples);
    }

    #[test]
    fn downmix_interleaved_averages_all_channels_to_mono() {
        // A stereo frame [1.0, 0.0] -> mono must give the average, 0.5.
        let samples = vec![1.0, 0.0, 0.5, 0.5];
        let mono = downmix_interleaved(&samples, 2, 1);
        assert_eq!(mono, vec![0.5, 0.5]);
    }

    #[test]
    fn downmix_interleaved_six_to_two_groups_even_and_odd_channels() {
        // Typical ffmpeg order for 5.1(side): L,R,C,LFE,Ls,Rs. With
        // an even index -> channel 0 (L,C,Ls) and odd -> channel 1
        // (R,LFE,Rs): a frame with L=1.0 and all the others at 0 must end up
        // almost entirely on channel 0 (average of 1.0,0.0,0.0 = 1/3), nothing
        // on channel 1.
        let l_only = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let stereo = downmix_interleaved(&l_only, 6, 2);
        assert_eq!(stereo.len(), 2);
        assert!((stereo[0] - (1.0 / 3.0)).abs() < 1e-6, "left={}", stereo[0]);
        assert_eq!(stereo[1], 0.0);
    }

    #[test]
    fn downmix_interleaved_upmixes_mono_by_duplicating_to_every_channel() {
        let mono = vec![0.7, -0.3];
        let stereo = downmix_interleaved(&mono, 1, 2);
        assert_eq!(stereo, vec![0.7, 0.7, -0.3, -0.3]);
    }

    #[test]
    fn downmix_interleaved_preserves_frame_count() {
        let samples = vec![0.0f32; 6 * 100]; // 100 frames at 6 channels
        let stereo = downmix_interleaved(&samples, 6, 2);
        assert_eq!(stereo.len(), 2 * 100);
    }

    #[test]
    fn db_to_linear_matches_known_reference_points() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // -6dB ~= halves the amplitude; +6dB ~= doubles it.
        assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
        // -20dB = exactly a factor of 0.1.
        assert!((db_to_linear(-20.0) - 0.1).abs() < 1e-6);
    }

    fn fade_test_clip(fade_in: u64, fade_out: u64) -> MixClip {
        MixClip {
            start: 0,
            len: 1000,
            source_offset: 0,
            buffer: Arc::new(Vec::new()),
            gain_db: Keyframed::constant(0.0),
            clip_fps: 10.0,
            fade_in,
            fade_out,
        }
    }

    #[test]
    fn fade_multiplier_ramps_linearly_in_and_out() {
        let clip = fade_test_clip(100, 200);
        assert_eq!(fade_multiplier(&clip, 0), 0.0);
        assert!((fade_multiplier(&clip, 50) - 0.5).abs() < 1e-6);
        assert_eq!(fade_multiplier(&clip, 100), 1.0);
        assert_eq!(fade_multiplier(&clip, 500), 1.0, "on the plateau it stays at full volume");
        assert!((fade_multiplier(&clip, 900) - 0.5).abs() < 1e-6, "200 samples from the end");
        assert_eq!(fade_multiplier(&clip, 1000), 0.0);
    }

    #[test]
    fn no_fade_stays_at_full_volume() {
        let clip = fade_test_clip(0, 0);
        assert_eq!(fade_multiplier(&clip, 0), 1.0);
        assert_eq!(fade_multiplier(&clip, 500), 1.0);
        assert_eq!(fade_multiplier(&clip, 1000), 1.0);
    }

    #[test]
    fn overlapping_fades_multiply_instead_of_dipping_below_either_ramp_alone() {
        // Fade in and out cover the whole clip: at the center each ramp is
        // 0.5, the product (not the minimum) is what one sees.
        let clip = fade_test_clip(1000, 1000);
        assert!((fade_multiplier(&clip, 500) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn fade_in_silences_the_start_of_a_block_and_full_gain_clip_is_unaffected() {
        let (project, a, _) = project();
        // 3 blocks of GAIN_BLOCK_FRAMES: the fade covers exactly the first one.
        let blocks = 3;
        let len_frames = (GAIN_BLOCK_FRAMES * blocks) as usize;
        let mut clip = clip_at(a, 0, 0, (len_frames / 10) as FrameIdx);
        clip.fade_in = (GAIN_BLOCK_FRAMES / 10) as FrameIdx;
        let tl = timeline(vec![audio_track(vec![clip])]);
        let snap = MixSnapshot::from_timeline(
            &project,
            &tl,
            RATE,
            1,
            |_, _| Some(Arc::new(vec![0.5; len_frames])),
            |_, _| None,
        );
        let mut out = vec![9.0; len_frames];
        mix_range(&snap, 0, &mut out);
        let block = GAIN_BLOCK_FRAMES as usize;
        assert!(out[..block].iter().all(|&s| s == 0.0), "first block silenced by the fade-in");
        assert!(
            out[block..].iter().all(|&s| s == 0.5),
            "past the fade-in the gain is unchanged"
        );
    }
}
