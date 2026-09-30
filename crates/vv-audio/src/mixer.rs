//! Continuous mixer of the timeline audio tracks: an immutable snapshot
//! of the clips (built by the UI thread) + `mix_range`, a pure function used
//! both by the cpal callback and by the export, so preview and export
//! produce the same mix.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vv_core::{
    AudioEffectKind, ChannelStrip, ClipSource, FrameIdx, Keyframed, MediaId, MixerChannel,
    MultibandCompressor, Project, Timeline, TimelineId,
};

use crate::dynamics::{BandActivity, MultibandProcessor};
use crate::spectrum::SpectrumTap;

pub const PROJECT_SAMPLE_RATE: u32 = 48_000;
/// Control-rate (~60Hz) granularity of the keyframed gain, not sample-accurate.
pub const GAIN_BLOCK_FRAMES: u64 = 800;
/// The mix goes track by track through buffers of this many frames.
const MIX_BLOCK_FRAMES: usize = 512;

#[derive(Clone)]
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
    /// Timeline index of the track: its entry in `MixSnapshot::tracks`.
    pub track: usize,
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

/// A mixer strip, ready for the mix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bus {
    pub gain: f32,
    /// Multipliers of the left and right channel.
    pub balance: [f32; 2],
}

impl Bus {
    pub const UNITY: Bus = Bus {
        gain: 1.0,
        balance: [1.0, 1.0],
    };

    pub fn from_strip(strip: &ChannelStrip) -> Self {
        Self {
            gain: db_to_linear(strip.gain_db),
            balance: balance_gains(strip.pan),
        }
    }

    /// Multiplier of output channel `c` of `channels`: the balance only
    /// touches the first two, and nothing in mono.
    fn channel_gain(&self, c: usize, channels: usize) -> f32 {
        if channels < 2 || c >= 2 {
            self.gain
        } else {
            self.gain * self.balance[c]
        }
    }
}

/// Balance, not a pan law: the centre keeps both channels at unity, moving
/// towards a side only attenuates the other one.
pub fn balance_gains(pan: f32) -> [f32; 2] {
    let pan = pan.clamp(-1.0, 1.0);
    [(1.0 - pan).min(1.0), (1.0 + pan).min(1.0)]
}

/// Peak of a left/right pair since the last `take`. Non-negative floats
/// order like their bits, so `fetch_max` works on them.
#[derive(Debug, Default)]
pub struct StereoPeak([AtomicU32; 2]);

impl StereoPeak {
    fn raise(&self, peak: [f32; 2]) {
        for (slot, value) in self.0.iter().zip(peak) {
            slot.fetch_max(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn take(&self) -> (f32, f32) {
        let [l, r] = &self.0;
        (
            f32::from_bits(l.swap(0, Ordering::Relaxed)),
            f32::from_bits(r.swap(0, Ordering::Relaxed)),
        )
    }
}

/// Post-fader peaks of what the output stream played.
#[derive(Debug, Default)]
pub struct MixMeters {
    /// Indexed like `MixSnapshot::tracks`.
    tracks: Vec<StereoPeak>,
    master: StereoPeak,
    /// Per channel and insert, like the chains of `MixSnapshot::tracks`.
    track_inserts: Vec<Vec<InsertMeter>>,
    master_inserts: Vec<InsertMeter>,
}

#[derive(Debug, Default)]
struct InsertMeter {
    bands: BandMeter,
    /// Compressors only.
    spectrum: Option<Arc<SpectrumTap>>,
}

impl MixMeters {
    fn new(tracks: &[Channel], master: &Channel) -> Self {
        let inserts = |channel: &Channel| {
            channel
                .inserts
                .iter()
                .map(|insert| InsertMeter {
                    bands: BandMeter::default(),
                    spectrum: matches!(insert, Insert::Compressor(_))
                        .then(|| Arc::new(SpectrumTap::default())),
                })
                .collect()
        };
        Self {
            tracks: tracks.iter().map(|_| StereoPeak::default()).collect(),
            master: StereoPeak::default(),
            track_inserts: tracks.iter().map(inserts).collect(),
            master_inserts: inserts(master),
        }
    }

    fn insert(&self, channel: MixerChannel, index: usize) -> Option<&InsertMeter> {
        match channel {
            MixerChannel::Track(track) => self.track_inserts.get(track)?.get(index),
            MixerChannel::Master => self.master_inserts.get(index),
        }
    }

    /// Takes over the spectra of `previous` where an effect is still there:
    /// a new snapshot for every edit would otherwise empty them all the time.
    pub fn keep_spectra_of(&mut self, previous: &MixMeters) {
        let pairs = self
            .track_inserts
            .iter_mut()
            .zip(&previous.track_inserts)
            .chain([(&mut self.master_inserts, &previous.master_inserts)]);
        for (chain, old_chain) in pairs {
            for (meter, old) in chain.iter_mut().zip(old_chain) {
                if let (Some(spectrum), Some(old)) = (&mut meter.spectrum, &old.spectrum) {
                    *spectrum = old.clone();
                }
            }
        }
    }

    /// Before and after the effect at `index` in the chain of `channel`.
    pub fn spectrum(&self, channel: MixerChannel, index: usize) -> Option<&SpectrumTap> {
        self.insert(channel, index)?.spectrum.as_deref()
    }

    pub fn track(&self, index: usize) -> Option<&StereoPeak> {
        self.tracks.get(index)
    }

    pub fn master(&self) -> &StereoPeak {
        &self.master
    }

    /// Of the effect at `index` in the chain of `channel`.
    pub fn band_meter(&self, channel: MixerChannel, index: usize) -> Option<&BandMeter> {
        Some(&self.insert(channel, index)?.bands)
    }

    fn track_inserts(&self, track: usize) -> &[InsertMeter] {
        self.track_inserts.get(track).map_or(&[], Vec::as_slice)
    }
}

/// Level and gain reduction of each band of a compressor since the last
/// `take`.
#[derive(Debug, Default)]
pub struct BandMeter {
    level: [AtomicU32; 3],
    reduction_db: [AtomicU32; 3],
}

impl BandMeter {
    fn raise(&self, activity: BandActivity) {
        let pairs = [
            (&self.level, activity.level),
            (&self.reduction_db, activity.reduction_db),
        ];
        for (slots, values) in pairs {
            for (slot, value) in slots.iter().zip(values) {
                slot.fetch_max(value.max(0.0).to_bits(), Ordering::Relaxed);
            }
        }
    }

    pub fn take(&self) -> BandActivity {
        let take = |slots: &[AtomicU32; 3]| {
            slots
                .each_ref()
                .map(|slot| f32::from_bits(slot.swap(0, Ordering::Relaxed)))
        };
        BandActivity {
            level: take(&self.level),
            reduction_db: take(&self.reduction_db),
        }
    }
}

/// An effect of a chain, ready for the mix: a normalization is the gain it
/// measured.
#[derive(Debug, Clone, PartialEq)]
pub enum Insert {
    /// A disabled effect, or one with nothing to act on: it keeps the
    /// inserts aligned with the effects of the strip.
    Off,
    Gain(f32),
    Compressor(MultibandCompressor),
    Mono,
}

/// A mixer strip, ready for the mix: its inserts, then fader and balance.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub inserts: Vec<Insert>,
    pub bus: Bus,
}

impl Channel {
    pub const UNITY: Channel = Channel {
        inserts: Vec::new(),
        bus: Bus::UNITY,
    };
}

impl Default for Channel {
    fn default() -> Self {
        Self::UNITY
    }
}

pub struct MixSnapshot {
    pub sample_rate: u32,
    pub channels: u16,
    /// Sorted by track.
    pub clips: Vec<MixClip>,
    /// Track and range of `clips` of each track with clips.
    groups: Vec<(usize, std::ops::Range<usize>)>,
    /// Indexed by timeline track index; a clip whose track is missing plays
    /// at unity.
    pub tracks: Vec<Channel>,
    pub master: Channel,
    pub meters: MixMeters,
}

impl MixSnapshot {
    pub fn empty(sample_rate: u32, channels: u16) -> Self {
        Self::new(
            sample_rate,
            channels,
            Vec::new(),
            Vec::new(),
            Channel::UNITY,
        )
    }

    pub fn new(
        sample_rate: u32,
        channels: u16,
        mut clips: Vec<MixClip>,
        tracks: Vec<Channel>,
        master: Channel,
    ) -> Self {
        clips.sort_by_key(|c| c.track);
        let mut groups: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        for (i, clip) in clips.iter().enumerate() {
            match groups.last_mut() {
                Some((track, range)) if *track == clip.track => range.end = i + 1,
                _ => groups.push((clip.track, i..i + 1)),
            }
        }
        let meters = MixMeters::new(&tracks, &master);
        Self {
            sample_rate,
            channels,
            clips,
            groups,
            tracks,
            master,
            meters,
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

    fn track(&self, index: usize) -> &Channel {
        static UNITY: Channel = Channel::UNITY;
        self.tracks.get(index).unwrap_or(&UNITY)
    }
}

/// The memory of the mix between two calls: the state of the effects that
/// have one, and where the previous call ended. A call that does not follow
/// on starts them over.
pub struct MixState {
    /// Aligned with the inserts of `MixSnapshot::tracks`.
    tracks: Vec<Vec<Option<MultibandProcessor>>>,
    master: Vec<Option<MultibandProcessor>>,
    scratch: Vec<f32>,
    peaks: Vec<[f32; 2]>,
    next: Option<u64>,
}

impl MixState {
    pub fn new(snapshot: &MixSnapshot) -> Self {
        let processors = |channel: &Channel| -> Vec<Option<MultibandProcessor>> {
            channel
                .inserts
                .iter()
                .map(|insert| match insert {
                    Insert::Off | Insert::Gain(_) | Insert::Mono => None,
                    Insert::Compressor(params) => Some(MultibandProcessor::new(
                        params,
                        snapshot.sample_rate,
                        snapshot.channels,
                    )),
                })
                .collect()
        };
        Self {
            tracks: snapshot.tracks.iter().map(processors).collect(),
            master: processors(&snapshot.master),
            scratch: vec![0.0; MIX_BLOCK_FRAMES * snapshot.channels.max(1) as usize],
            peaks: vec![[0.0; 2]; snapshot.tracks.len()],
            next: None,
        }
    }

    /// Carries on from the state of the previous snapshot: the effects
    /// found at the same place keep their memory, so an edit does not
    /// restart them.
    pub fn continue_from(&mut self, previous: &MixState) {
        let pairs = self
            .tracks
            .iter_mut()
            .zip(&previous.tracks)
            .chain([(&mut self.master, &previous.master)]);
        for (chain, old_chain) in pairs {
            for (processor, old) in chain.iter_mut().zip(old_chain) {
                if let (Some(processor), Some(old)) = (processor, old) {
                    processor.take_memory_from(old);
                }
            }
        }
        self.next = previous.next;
    }

    fn reset(&mut self) {
        for processor in self
            .tracks
            .iter_mut()
            .chain([&mut self.master])
            .flatten()
            .flatten()
        {
            processor.reset();
        }
    }
}

/// The channel a normalization measures: while a new reading is on its way,
/// the same slot keeps playing with its last one.
/// The channel and the position of the normalization in its chain.
pub type AnalysisSlot = (TimelineId, MixerChannel, usize);

/// A part of the mix whose peak a normalization needs.
pub struct PeakAnalysis {
    pub slot: Option<AnalysisSlot>,
    snapshot: MixSnapshot,
}

impl PeakAnalysis {
    /// Same key, same content, same peak.
    pub fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let s = &self.snapshot;
        (s.sample_rate, s.channels).hash(&mut h);
        for clip in &s.clips {
            (
                clip.start,
                clip.len,
                clip.source_offset,
                clip.step.to_bits(),
            )
                .hash(&mut h);
            (Arc::as_ptr(&clip.buffer) as usize, clip.buffer.len()).hash(&mut h);
            if clip.gain_db.is_constant() {
                clip.gain_db.default.to_bits().hash(&mut h);
            } else {
                format!("{:?}", clip.gain_db).hash(&mut h);
            }
            (clip.track, clip.clip_fps.to_bits(), clip.media_offset).hash(&mut h);
            (clip.media_step.to_bits(), clip.fade_in, clip.fade_out).hash(&mut h);
        }
        for channel in s.tracks.iter().chain([&s.master]) {
            let bus = &channel.bus;
            (bus.gain.to_bits(), bus.balance.map(f32::to_bits)).hash(&mut h);
            for insert in &channel.inserts {
                hash_insert(insert, &mut h);
            }
        }
        h.finish()
    }

    /// Linear peak over all the channels. Slow: it renders the whole mix.
    pub fn run(&self) -> f32 {
        const CHUNK_FRAMES: u64 = 1 << 16;
        let clips = &self.snapshot.clips;
        let start = clips.iter().map(|c| c.start).min().unwrap_or(0);
        let end = clips.iter().map(|c| c.start + c.len).max().unwrap_or(0);
        let ch = self.snapshot.channels.max(1) as usize;
        let mut out = vec![0.0f32; CHUNK_FRAMES as usize * ch];
        let mut state = MixState::new(&self.snapshot);
        let mut peak = 0.0f32;
        let mut f = start;
        while f < end {
            let frames = (end - f).min(CHUNK_FRAMES);
            let chunk = &mut out[..frames as usize * ch];
            mix_into(&self.snapshot, &mut state, f, chunk, false);
            peak = chunk.iter().fold(peak, |p, s| p.max(s.abs()));
            f += frames;
        }
        peak
    }
}

fn hash_insert(insert: &Insert, h: &mut impl Hasher) {
    match insert {
        Insert::Off => 2u8.hash(h),
        Insert::Mono => 3u8.hash(h),
        Insert::Gain(gain) => (0u8, gain.to_bits()).hash(h),
        Insert::Compressor(params) => {
            (1u8, params.crossovers_hz.map(f32::to_bits)).hash(h);
            for band in &params.bands {
                let fields = [
                    band.threshold_db,
                    band.ratio,
                    band.attack_ms,
                    band.release_ms,
                    band.makeup_db,
                ];
                fields.map(f32::to_bits).hash(h);
            }
        }
    }
}

pub enum PeakReading {
    Ready(f32),
    /// Being measured; the last reading of the same slot, if any.
    Pending(Option<f32>),
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

    /// Synchronous here; the preview measures in the background.
    fn peak(&mut self, analysis: PeakAnalysis) -> PeakReading {
        PeakReading::Ready(analysis.run())
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
    for (track_index, track) in timeline.audible_tracks() {
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
            mix_clip.track = track_index;
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
    let timeline_id = project
        .timelines
        .iter()
        .find(|(_, t)| std::ptr::eq(*t, timeline))
        .map(|(id, _)| id);
    let slot = |channel| timeline_id.map(|id| (id, channel));
    let mut tracks: Vec<Channel> = Vec::with_capacity(timeline.tracks.len());
    for (index, track) in timeline.tracks.iter().enumerate() {
        let track_clips: Vec<MixClip> =
            clips.iter().filter(|c| c.track == index).cloned().collect();
        // Measured alone and before the fader: only this track has an entry.
        let measure = |inserts: &[Insert]| {
            let mut tracks = vec![Channel::UNITY; index + 1];
            tracks[index].inserts = inserts.to_vec();
            MixSnapshot::new(
                sample_rate,
                channels,
                track_clips.clone(),
                tracks,
                Channel::UNITY,
            )
        };
        let inserts = resolve_chain(
            &track.mix,
            !track_clips.is_empty(),
            slot(MixerChannel::Track(index)),
            measure,
            source,
            &mut readiness,
        );
        tracks.push(Channel {
            inserts,
            bus: Bus::from_strip(&track.mix),
        });
    }
    let measure = |inserts: &[Insert]| {
        let master = Channel {
            inserts: inserts.to_vec(),
            bus: Bus::UNITY,
        };
        MixSnapshot::new(sample_rate, channels, clips.clone(), tracks.clone(), master)
    };
    let master = Channel {
        inserts: resolve_chain(
            &timeline.master,
            !clips.is_empty(),
            slot(MixerChannel::Master),
            measure,
            source,
            &mut readiness,
        ),
        bus: Bus::from_strip(&timeline.master),
    };
    let snapshot = MixSnapshot::new(sample_rate, channels, clips, tracks, master);
    (snapshot, readiness)
}

/// The effects of `strip`, in order, one insert each. A normalization
/// measures the channel through the effects before it (`measure` builds that
/// mix), so it stays right after a compressor too.
fn resolve_chain(
    strip: &ChannelStrip,
    has_audio: bool,
    slot: Option<(TimelineId, MixerChannel)>,
    measure: impl Fn(&[Insert]) -> MixSnapshot,
    source: &mut impl AudioSource,
    readiness: &mut Readiness,
) -> Vec<Insert> {
    let mut inserts = Vec::new();
    for effect in &strip.effects {
        if !effect.enabled {
            inserts.push(Insert::Off);
            continue;
        }
        match &effect.kind {
            AudioEffectKind::Normalize { .. } if !has_audio => inserts.push(Insert::Off),
            AudioEffectKind::Normalize { target_db } => {
                let reading = source.peak(PeakAnalysis {
                    slot: slot.map(|(timeline, channel)| (timeline, channel, inserts.len())),
                    snapshot: measure(&inserts),
                });
                inserts.push(Insert::Gain(normalize_gain(*target_db, reading, readiness)));
            }
            AudioEffectKind::MultibandCompressor(params) => {
                inserts.push(Insert::Compressor(params.clone()));
            }
            AudioEffectKind::Mono => inserts.push(Insert::Mono),
        }
    }
    inserts
}

/// A reading still on its way leaves the mix `Partial`, so a compound
/// containing it is not cached with the wrong gain.
fn normalize_gain(target_db: f32, reading: PeakReading, readiness: &mut Readiness) -> f32 {
    let peak = match reading {
        PeakReading::Ready(peak) => Some(peak),
        PeakReading::Pending(last) => {
            *readiness = (*readiness).max(Readiness::Partial);
            last
        }
    };
    match peak {
        // Silence stays silence, instead of the maximum gain.
        Some(peak) if peak > 1e-6 => {
            (db_to_linear(target_db) / peak).min(db_to_linear(vv_core::GAIN_DB_MAX))
        }
        _ => 1.0,
    }
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
        track: 0,
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
/// starting from timeline audio frame `start`, with the effects starting
/// from scratch.
pub fn mix_range(snapshot: &MixSnapshot, start: u64, out: &mut [f32]) {
    mix_into(snapshot, &mut MixState::new(snapshot), start, out, false);
}

/// `mix_range` for the output stream: the effects carry on from the
/// previous call through `state`, and `snapshot.meters` go up. No
/// allocations: it runs in the realtime callback.
pub fn mix_range_metered(
    snapshot: &MixSnapshot,
    state: &mut MixState,
    start: u64,
    out: &mut [f32],
) {
    mix_into(snapshot, state, start, out, true);
}

fn mix_into(
    snapshot: &MixSnapshot,
    state: &mut MixState,
    start: u64,
    out: &mut [f32],
    metered: bool,
) {
    out.fill(0.0);
    let ch = snapshot.channels as usize;
    if ch == 0 {
        return;
    }
    let frames = out.len() / ch;
    if state.next != Some(start) {
        state.reset();
    }
    state.next = Some(start + frames as u64);
    state.peaks.fill([0.0; 2]);
    let mut master_peak = [0.0f32; 2];
    for block_start in (0..frames).step_by(MIX_BLOCK_FRAMES) {
        let len = MIX_BLOCK_FRAMES.min(frames - block_start);
        let block = &mut out[block_start * ch..(block_start + len) * ch];
        let block_time = start + block_start as u64;
        for (track, range) in &snapshot.groups {
            let channel = snapshot.track(*track);
            let scratch = &mut state.scratch[..len * ch];
            scratch.fill(0.0);
            for clip in &snapshot.clips[range.clone()] {
                add_clip(snapshot, clip, block_time, scratch);
            }
            let processors = state.tracks.get_mut(*track).map(Vec::as_mut_slice);
            let meters = metered.then(|| snapshot.meters.track_inserts(*track));
            apply_inserts(
                &channel.inserts,
                processors.unwrap_or_default(),
                scratch,
                ch,
                meters,
            );
            let mut peak = [0.0f32; 2];
            for (dst, src) in block.chunks_exact_mut(ch).zip(scratch.chunks_exact(ch)) {
                for (c, (d, s)) in dst.iter_mut().zip(src).enumerate() {
                    let value = s * channel.bus.channel_gain(c, ch);
                    *d += value;
                    if c < 2 {
                        peak[c] = peak[c].max(value.abs());
                    }
                }
            }
            if let Some(track_peak) = state.peaks.get_mut(*track) {
                *track_peak = [track_peak[0].max(peak[0]), track_peak[1].max(peak[1])];
            }
        }
        let meters = metered.then_some(snapshot.meters.master_inserts.as_slice());
        apply_inserts(
            &snapshot.master.inserts,
            &mut state.master,
            block,
            ch,
            meters,
        );
        for frame in block.chunks_exact_mut(ch) {
            for (c, s) in frame.iter_mut().enumerate() {
                *s *= snapshot.master.bus.channel_gain(c, ch);
                if c < 2 {
                    master_peak[c] = master_peak[c].max(s.abs());
                }
            }
        }
    }
    if metered {
        for (track, peak) in state.peaks.iter().enumerate() {
            if let Some(meter) = snapshot.meters.track(track) {
                meter.raise(mono_as_stereo(*peak, ch));
            }
        }
        snapshot
            .meters
            .master
            .raise(mono_as_stereo(master_peak, ch));
    }
}

/// `meters`: where the compressors report, if metered.
fn apply_inserts(
    inserts: &[Insert],
    processors: &mut [Option<MultibandProcessor>],
    samples: &mut [f32],
    channels: usize,
    meters: Option<&[InsertMeter]>,
) {
    for (index, insert) in inserts.iter().enumerate() {
        match insert {
            Insert::Off => {}
            Insert::Gain(gain) => samples.iter_mut().for_each(|s| *s *= gain),
            Insert::Mono => {
                for frame in samples.chunks_exact_mut(channels.max(1)) {
                    let average = frame.iter().sum::<f32>() / frame.len() as f32;
                    frame.fill(average);
                }
            }
            Insert::Compressor(_) => {
                let Some(Some(processor)) = processors.get_mut(index) else {
                    continue;
                };
                let meter = meters.and_then(|m| m.get(index));
                let spectrum = meter.and_then(|m| m.spectrum.as_deref());
                if let Some(spectrum) = spectrum {
                    spectrum.record_pre(samples, channels);
                }
                let activity = processor.process(samples);
                if let Some(spectrum) = spectrum {
                    spectrum.record_post(samples, channels);
                }
                if let Some(meter) = meter {
                    meter.bands.raise(activity);
                }
            }
        }
    }
}

/// Adds the part of `clip` from timeline audio frame `start` into `out`,
/// with its gain and fades.
fn add_clip(snapshot: &MixSnapshot, clip: &MixClip, start: u64, out: &mut [f32]) {
    let ch = snapshot.channels as usize;
    let end = start + (out.len() / ch) as u64;
    let to = end.min(clip.start + clip.len);
    let mut f = start.max(clip.start);
    while f < to {
        let in_clip = f - clip.start;
        let block = in_clip / GAIN_BLOCK_FRAMES;
        let block_end = (clip.start + (block + 1) * GAIN_BLOCK_FRAMES).min(to);
        // At the start of the block, not of the call: how the mix is split
        // into calls must not change it.
        let gain = block_gain_linear(clip, block, snapshot.sample_rate)
            * fade_multiplier(clip, block * GAIN_BLOCK_FRAMES);
        let dst = ((f - start) as usize) * ch;
        if clip.step == 1.0 {
            let src = ((clip.source_offset + in_clip) as usize) * ch;
            let count = ((block_end - f) as usize * ch).min(clip.buffer.len().saturating_sub(src));
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

fn mono_as_stereo(peak: [f32; 2], channels: usize) -> [f32; 2] {
    if channels == 1 { [peak[0]; 2] } else { peak }
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
    /// Locked only by the callback; allocated here, on the UI thread.
    effects: Mutex<MixState>,
}

impl MixerState {
    pub fn new(mix: Arc<MixSnapshot>, stretched: Option<StretchedWindow>) -> Self {
        let effects = Mutex::new(MixState::new(&mix));
        Self {
            mix,
            stretched,
            effects,
        }
    }
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
        let initial = Arc::new(MixerState::new(
            Arc::new(MixSnapshot::empty(sample_rate, channels)),
            None,
        ));
        let pending: Arc<Mutex<Option<Arc<MixerState>>>> = Arc::new(Mutex::new(None));
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
                        if let (Ok(mut effects), Ok(previous)) =
                            (next.effects.try_lock(), current.effects.try_lock())
                        {
                            effects.continue_from(&previous);
                        }
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
                                match current.effects.try_lock() {
                                    Ok(mut effects) => {
                                        mix_range_metered(&current.mix, &mut effects, pos, data)
                                    }
                                    Err(_) => data.fill(0.0),
                                }
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
