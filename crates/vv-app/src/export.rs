//! Export: walks the timeline and writes H.264+AAC with the same layers and
//! the same mix as the preview. Runs on a dedicated thread, on a
//! snapshot of the project. `EffectStack::speed` is not applied yet.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vv_core::{
    Clip, ClipId, ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId, TrackKind,
};

use vv_audio::mixer::{
    MixSnapshot, PROJECT_SAMPLE_RATE, mix_range, remix_channels_into, timeline_frame_to_sample,
};

use crate::frame_provider::{FrameProvider, GpuCompounds, OwnedLayer, media_source_frame, track_layers_at};

const PROJECT_CHANNELS: u16 = 2;
const RENDER_AHEAD_FRAMES: usize = 8;
/// Same limit and same reason as `MAX_COMPOUND_DEPTH` in
/// `render_ahead.rs`: since the project timeline is in the media pool
/// too, a cycle of compound clips is possible.
const MAX_COMPOUND_DEPTH: u32 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportSettings {
    pub output_path: PathBuf,
    /// Percentage of the timeline resolution.
    pub scale_percent: u32,
    pub video: vv_media::VideoSettings,
    /// `None` = export without audio.
    pub audio: Option<vv_media::AudioSettings>,
}

impl ExportSettings {
    pub const SCALE_CHOICES: [u32; 4] = [100, 75, 50, 25];

    pub fn new(output_path: PathBuf) -> Self {
        Self {
            output_path,
            scale_percent: 100,
            video: vv_media::VideoSettings::default(),
            audio: Some(vv_media::AudioSettings::default()),
        }
    }

    /// Like `new`, but with the fastest encoders available on this
    /// machine (NVENC, FDK). `new` stays deterministic for the tests.
    pub fn preferred(output_path: PathBuf) -> Self {
        let mut settings = Self::new(output_path);
        if vv_media::VideoCodec::Nvenc.is_available() {
            settings.video.codec = vv_media::VideoCodec::Nvenc;
            settings.video.preset = settings.video.codec.default_preset().into();
        }
        if vv_media::AudioCodec::FdkAac.is_available()
            && let Some(audio) = &mut settings.audio
        {
            audio.codec = vv_media::AudioCodec::FdkAac;
        }
        settings
    }

    /// Reduced to even dimensions (required by the encoders' 4:2:0);
    /// at 100% it stays exactly the timeline's.
    pub fn output_size(&self, timeline_size: (u32, u32)) -> (u32, u32) {
        if self.scale_percent >= 100 {
            return timeline_size;
        }
        let scale = |v: u32| ((v * self.scale_percent / 100) & !1).max(2);
        (scale(timeline_size.0), scale(timeline_size.1))
    }
}

#[derive(Default)]
pub struct ExportProgress {
    pub current_frame: FrameIdx,
    pub total_frames: FrameIdx,
    pub done: bool,
    pub error: Option<String>,
    pub elapsed: std::time::Duration,
}

/// Decoder kept open for the active video clip, with a seek/reopen only
/// when the clip changes (not a new decoder for every output frame:
/// every seek is a flush to a keyframe, too slow to do on every frame).
struct ActiveClipDecoder {
    decoder: vv_media::Decoder,
    /// A conformed clip asks for the same source frame on consecutive
    /// timeline frames, and the decoder does not go back.
    last: Option<(FrameIdx, Arc<vv_media::FrameYuv420>)>,
}

impl ActiveClipDecoder {
    fn open_for(path: &Path, target_source_frame: FrameIdx, is_image: bool) -> Result<Self, String> {
        // `Decoder::open` on an image would hit EOF after the first frame.
        let mut decoder = if is_image {
            vv_media::Decoder::open_image(path)
        } else {
            vv_media::Decoder::open(path)
        }
        .map_err(|e| e.to_string())?;
        let secs = target_source_frame as f64 / decoder.fps().as_f64().max(1e-9);
        decoder.seek_to_time(secs).map_err(|e| e.to_string())?;
        let mut me = Self {
            decoder,
            last: None,
        };
        me.advance_to(target_source_frame)?;
        Ok(me)
    }

    /// Decodes forward up to `target`; if already reached it returns the last
    /// frame again. `None` at the end of the stream.
    fn advance_to(
        &mut self,
        target: FrameIdx,
    ) -> Result<Option<Arc<vv_media::FrameYuv420>>, String> {
        if let Some((idx, frame)) = &self.last
            && *idx >= target
        {
            return Ok(Some(frame.clone()));
        }
        loop {
            match self.decoder.next_frame().map_err(|e| e.to_string())? {
                Some((idx, frame)) if idx >= target => {
                    let frame = Arc::new(frame);
                    self.last = Some((idx, frame.clone()));
                    return Ok(Some(frame));
                }
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// Decoders kept open from one frame to the next, reopened only when the
/// clip changes.
#[derive(Default)]
struct StreamingFrameProvider {
    /// One per clip: several tracks can be active on the same frame.
    /// Pruned by `retain_clips`.
    active: HashMap<ClipId, ActiveClipDecoder>,
}

impl StreamingFrameProvider {
    fn retain_clips(&mut self, keep: &[ClipId]) {
        self.active.retain(|id, _| keep.contains(id));
    }
}

impl FrameProvider for StreamingFrameProvider {
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<vv_media::FrameYuv420>>, String> {
        let Some((media_id, source_frame)) = media_source_frame(clip, timeline_frame) else {
            self.active.remove(&clip.id);
            return Ok(None);
        };
        let item = project
            .media_pool
            .get(media_id)
            .ok_or_else(|| t!("export.error_media_not_found").into_owned())?;

        // A compound clip is not decoded: `GpuCompounds` composes it.
        // Getting here means it stopped at the nesting limit —
        // no layer for this clip, as for a missing media.
        if item.compound.is_some() {
            return Ok(None);
        }

        let path = item.path.clone();
        let is_image = item.meta.is_image();

        let decoder = match self.active.entry(clip.id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(ActiveClipDecoder::open_for(&path, source_frame, is_image)?),
        };
        decoder.advance_to(source_frame)
    }
}

/// Exports the frames in `range` to `output_path`. Blocking.
pub fn export_timeline(
    project: &Project,
    timeline_id: TimelineId,
    settings: &ExportSettings,
    range: std::ops::Range<FrameIdx>,
    progress: &Mutex<ExportProgress>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let finish = || {
        let mut p = progress.lock().unwrap();
        p.elapsed = started.elapsed();
        p.done = true;
    };
    let timeline = project
        .timelines
        .get(timeline_id)
        .ok_or_else(|| t!("export.error_timeline_not_found").into_owned())?;

    let range = range.start.max(0)..range.end.min(timeline.total_frames());
    let total_frames = (range.end - range.start).max(0);
    progress.lock().unwrap().total_frames = total_frames;
    if total_frames <= 0 {
        finish();
        return Ok(());
    }

    let audio_settings = settings.audio.as_ref().filter(|_| {
        timeline
            .tracks_of_kind(TrackKind::Audio)
            .any(|(_, t)| !t.clips.is_empty())
    });
    let has_audio_track = audio_settings.is_some();

    let (out_w, out_h) = settings.output_size(timeline.resolution);
    let mut encoder = vv_media::Encoder::new(
        &settings.output_path,
        out_w,
        out_h,
        timeline.fps,
        &settings.video,
        audio_settings.map(|a| (PROJECT_SAMPLE_RATE, PROJECT_CHANNELS, a)),
    )
    .map_err(|e| e.to_string())?;

    // Decode, GPU composition and encode on three threads: in series each one
    // waited for the others and none saturated the machine.
    let (decoded_tx, decoded_rx) =
        std::sync::mpsc::sync_channel::<Result<Vec<OwnedLayer>, String>>(RENDER_AHEAD_FRAMES);
    let (composed_tx, composed_rx) =
        std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(RENDER_AHEAD_FRAMES);
    let resolution = timeline.resolution;
    let output = vv_render::OutputFrame::scaled(out_w, out_h, resolution);
    // A single one for the two GPU stages: the texture of a compound clip is born
    // in the decode stage and sampled in the composition one,
    // so they must be on the same device. Headless, so as not to contend
    // with the UI's.
    let compositor = vv_render::Compositor::new_headless();
    let compositor = &compositor;
    std::thread::scope(|scope| {
        let mut audio_mix = has_audio_track.then(|| {
            let range = range.clone();
            scope.spawn(move || mix_audio_track(project, timeline, range))
        });

        let decode_range = range.clone();
        scope.spawn(move || {
            let mut provider = StreamingFrameProvider::default();
            for frame in decode_range {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let decoded =
                    decode_video_frame(project, timeline, &mut provider, compositor, frame, resolution);
                let failed = decoded.is_err();
                // `send` fails only if the next stage has already stopped.
                if decoded_tx.send(decoded).is_err() || failed {
                    return;
                }
            }
        });

        scope.spawn(move || {
            for decoded in decoded_rx {
                let composed =
                    decoded.map(|layers| compose_video_frame(compositor, &layers, output));
                let failed = composed.is_err();
                if composed_tx.send(composed).is_err() || failed {
                    return;
                }
            }
        });

        // Inside the closure: if the encoder exits with an error `composed_rx` must
        // be closed before the join, or the upstream stages stay on `send`.
        let composed_rx = composed_rx;
        // The audio must be written along with the video: all of it after the video and
        // the muxer keeps the whole video in RAM and the interleaving becomes
        // quadratic (minutes on a timeline of a few minutes).
        let mut audio = AudioInterleaver::new(timeline, range.start);
        for frame in range.clone() {
            if cancel.load(Ordering::Relaxed) {
                return Err(t!("export.cancelled").into_owned());
            }
            let frame_i420 = match composed_rx.recv() {
                Ok(frame_i420) => frame_i420?,
                Err(_) => return Err(t!("export.cancelled").into_owned()),
            };
            encoder
                .write_video_frame(&frame_i420)
                .map_err(|e| e.to_string())?;
            if audio_mix.as_ref().is_some_and(|h| h.is_finished()) {
                audio.mixed = Some(join_audio_mix(audio_mix.take())?);
            }
            audio.write_until(&mut encoder, frame + 1)?;

            progress.lock().unwrap().current_frame = frame - range.start + 1;
        }
        if audio_mix.is_some() {
            audio.mixed = Some(join_audio_mix(audio_mix)?);
        }
        audio.write_until(&mut encoder, range.end)
    })?;

    encoder.finish().map_err(|e| e.to_string())?;
    finish();
    Ok(())
}

#[cfg(test)]
fn render_video_frame(
    project: &Project,
    timeline: &Timeline,
    compositor: &vv_render::Compositor,
    provider: &mut StreamingFrameProvider,
    frame: FrameIdx,
    resolution: (u32, u32),
) -> Result<Vec<u8>, String> {
    let layers = decode_video_frame(project, timeline, provider, compositor, frame, resolution)?;
    let output = vv_render::OutputFrame::exact(resolution.0, resolution.1);
    Ok(compose_video_frame(compositor, &layers, output))
}

/// The layers of the frame, from bottom to top. A missing media frame
/// (past the real end of the file) leaves out only that layer.
fn decode_video_frame(
    project: &Project,
    timeline: &Timeline,
    provider: &mut StreamingFrameProvider,
    compositor: &vv_render::Compositor,
    frame: FrameIdx,
    resolution: (u32, u32),
) -> Result<Vec<OwnedLayer>, String> {
    let clips = timeline.active_video_clips_at(frame);
    // In addition to the "naturally" active clips, the other half of a
    // crossing transition in progress too: `track_layers_at` decodes it as well,
    // otherwise `retain_clips` would close it on every frame as soon as it is opened.
    let mut keep: Vec<ClipId> = clips.iter().map(|(_, c)| c.id).collect();
    for &(track_index, _) in &clips {
        if let Some((left, right, _)) = timeline.tracks[track_index].crossing_at(frame) {
            keep.push(left.id);
            keep.push(right.id);
        }
    }
    provider.retain_clips(&keep);
    let mut gpu = GpuCompounds::new(provider, compositor);
    let mut layers = Vec::with_capacity(clips.len());
    for (track_index, clip) in clips {
        layers.extend(track_layers_at(project, timeline, track_index, clip, frame, resolution, &mut gpu)?);
    }
    Ok(layers)
}

fn compose_video_frame(
    compositor: &vv_render::Compositor,
    layers: &[OwnedLayer],
    output: vv_render::OutputFrame,
) -> Vec<u8> {
    let layers: Vec<vv_render::Layer> = layers.iter().map(OwnedLayer::as_render).collect();
    compositor.render_layers_i420(&layers, output)
}

fn join_audio_mix(
    handle: Option<std::thread::ScopedJoinHandle<'_, Result<Vec<f32>, String>>>,
) -> Result<Vec<f32>, String> {
    handle
        .expect("mix audio già consumato")
        .join()
        .expect("thread di mix audio andato in panic")
}

/// Writes the audio mix to the encoder in pieces, aligned to the video frames.
struct AudioInterleaver {
    mixed: Option<Vec<f32>>,
    written: usize,
    fps: f64,
    start_sample: u64,
}

impl AudioInterleaver {
    fn new(timeline: &Timeline, start_frame: FrameIdx) -> Self {
        let fps = timeline.fps.as_f64();
        Self {
            mixed: None,
            written: 0,
            fps,
            start_sample: timeline_frame_to_sample(start_frame, fps, PROJECT_SAMPLE_RATE),
        }
    }

    /// Writes the samples up to the start of `frame` (exclusive); no-op
    /// until the mix is ready.
    fn write_until(
        &mut self,
        encoder: &mut vv_media::Encoder,
        frame: FrameIdx,
    ) -> Result<(), String> {
        let Some(mixed) = &self.mixed else {
            return Ok(());
        };
        let end_sample = timeline_frame_to_sample(frame, self.fps, PROJECT_SAMPLE_RATE);
        let end = ((end_sample - self.start_sample) as usize * PROJECT_CHANNELS as usize)
            .min(mixed.len());
        if end > self.written {
            encoder
                .write_audio_samples(&mixed[self.written..end])
                .map_err(|e| e.to_string())?;
            self.written = end;
        }
        Ok(())
    }
}

/// Mix of all the audio tracks at `PROJECT_SAMPLE_RATE`/`PROJECT_CHANNELS`,
/// over the timeline frames in `range`: the same `mix_range` as the preview.
fn mix_audio_track(
    project: &Project,
    timeline: &Timeline,
    range: std::ops::Range<FrameIdx>,
) -> Result<Vec<f32>, String> {
    // The same decoding as the preview (`mix_buffers`): swresample to
    // `PROJECT_SAMPLE_RATE`, all the streams of a file in one pass.
    // Recursive: the real media inside a compound clip end up in the
    // same collection, as if they were clips of `timeline`.
    let mut streams_by_path: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    collect_audio_streams(project, timeline, &mut streams_by_path, 0);
    let mut buffers: HashMap<(PathBuf, usize), Arc<Vec<f32>>> = HashMap::new();
    for (path, streams) in streams_by_path {
        let mut decoded = vec![Vec::new(); streams.len()];
        let formats = vv_media::decode_audio_streams_streaming(
            &path,
            &streams,
            Some(PROJECT_SAMPLE_RATE),
            |slot, channels, chunk| {
                remix_channels_into(chunk, channels, PROJECT_CHANNELS, &mut decoded[slot]);
                ControlFlow::Continue(())
            },
        )
        .map_err(|e| e.to_string())?;
        for ((stream, samples), format) in streams.into_iter().zip(decoded).zip(formats) {
            if format.is_some() {
                buffers.insert((path.clone(), stream), Arc::new(samples));
            }
        }
    }

    let snapshot = MixSnapshot::from_timeline(
        project,
        timeline,
        PROJECT_SAMPLE_RATE,
        PROJECT_CHANNELS,
        |path, stream| buffers.get(&(path.to_path_buf(), stream)).cloned(),
        |_, media_id| compound_mix_buffer(project, media_id, &buffers, 0),
    );
    let fps = timeline.fps.as_f64();
    let start_sample = timeline_frame_to_sample(range.start, fps, PROJECT_SAMPLE_RATE);
    let end_sample = timeline_frame_to_sample(range.end, fps, PROJECT_SAMPLE_RATE);
    let mut mixed =
        vec![0.0_f32; (end_sample - start_sample) as usize * PROJECT_CHANNELS as usize];
    mix_range(&snapshot, start_sample, &mut mixed);
    Ok(mixed)
}

/// Every Media clip of `timeline` (at any nesting depth inside the
/// compound clips) referencing a real file, collected into
/// `streams_by_path`: a compound clip itself does not generate an entry (its
/// "file" does not exist), only what its nested timeline references.
fn collect_audio_streams(
    project: &Project,
    timeline: &Timeline,
    streams_by_path: &mut HashMap<PathBuf, Vec<usize>>,
    depth: u32,
) {
    if depth >= MAX_COMPOUND_DEPTH {
        return;
    }
    for (_, track) in timeline.audible_tracks() {
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let Some(item) = project.media_pool.get(*media_id) else {
                continue;
            };
            match item.compound {
                Some(nested_id) => {
                    if let Some(nested) = project.timelines.get(nested_id) {
                        collect_audio_streams(project, nested, streams_by_path, depth + 1);
                    }
                }
                None => {
                    let streams = streams_by_path.entry(item.path.clone()).or_default();
                    if !streams.contains(&clip.audio_stream_index) {
                        streams.push(clip.audio_stream_index);
                    }
                }
            }
        }
    }
}

/// The mixdown of the nested timeline of a compound clip, at
/// `PROJECT_SAMPLE_RATE`/`PROJECT_CHANNELS`: no caching is needed here (the export
/// asks for it once for the whole export) nor checking that the real media
/// are ready (`buffers` already has them all, decoded before getting
/// here by `collect_audio_streams`). Recursive for a compound clip
/// inside another.
fn compound_mix_buffer(
    project: &Project,
    media_id: MediaId,
    buffers: &HashMap<(PathBuf, usize), Arc<Vec<f32>>>,
    depth: u32,
) -> Option<Arc<Vec<f32>>> {
    if depth >= MAX_COMPOUND_DEPTH {
        return None;
    }
    let item = project.media_pool.get(media_id)?;
    let nested = project.timelines.get(item.compound?)?;
    let snapshot = MixSnapshot::from_timeline(
        project,
        nested,
        PROJECT_SAMPLE_RATE,
        PROJECT_CHANNELS,
        |path, stream| buffers.get(&(path.to_path_buf(), stream)).cloned(),
        |_, inner_media_id| compound_mix_buffer(project, inner_media_id, buffers, depth + 1),
    );
    let len = snapshot.clips.iter().map(|c| c.start + c.len).max().unwrap_or(0);
    let mut buffer = vec![0.0_f32; len as usize * PROJECT_CHANNELS as usize];
    mix_range(&snapshot, 0, &mut buffer);
    Some(Arc::new(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{Clip, Keyframed, Rgba, Track, TrackKind};

    fn solid_color_clip(id: u64, start: FrameIdx, len: FrameIdx, color: Rgba) -> Clip {
        let mut clip = Clip::from_source_range(
            ClipId(id),
            ClipSource::SolidColor,
            0,
            len,
            start,
            vv_core::Rational::one(),
        );
        clip.effects.color = Some(Keyframed::constant(color));
        clip
    }

    fn timeline_with(tracks: Vec<Track>) -> Timeline {
        Timeline {
            name: "t".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (4, 2),
            tracks,
        }
    }

    // BT.709 limited range, like `Compositor::render_layers_i420`.
    const BLACK_I420: [u8; 3] = [16, 128, 128];
    const RED_I420: [u8; 3] = [63, 102, 240];
    const BLUE_I420: [u8; 3] = [32, 240, 118];

    fn solid_i420(width: usize, height: usize, [y, u, v]: [u8; 3]) -> Vec<u8> {
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        let mut data = vec![y; width * height];
        data.extend(std::iter::repeat_n(u, chroma));
        data.extend(std::iter::repeat_n(v, chroma));
        data
    }

    fn red() -> Rgba {
        Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }
    }

    fn blue() -> Rgba {
        Rgba {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        }
    }

    #[test]
    fn render_video_frame_returns_black_in_a_gap() {
        let project = Project::default();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 10, 5, red())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame = render_video_frame(&project, &tl, &compositor, &mut active, 0, (2, 2)).unwrap();
        assert_eq!(frame, solid_i420(2, 2, BLACK_I420));
    }

    #[test]
    fn render_video_frame_reads_solid_color_at_the_clips_source_frame() {
        let project = Project::default();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 10, 5, red())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame =
            render_video_frame(&project, &tl, &compositor, &mut active, 12, (2, 2)).unwrap();
        assert_eq!(frame, solid_i420(2, 2, RED_I420));
    }

    /// A video clip inside the nested timeline of a compound clip must
    /// show up in the exported frame, at the position of the compound clip
    /// in the outer timeline — not before/after, and not at the one it
    /// would have in its nested timeline.
    #[test]
    fn render_video_frame_recurses_into_a_compound_clips_nested_timeline() {
        let mut project = Project::default();
        let nested_id = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (2, 2),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 10, red())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            }],
        });
        let compound_media = project.media_pool.insert(vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 10,
                fps: vv_core::Rational::new(25, 1),
                width: 2,
                height: 2,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(nested_id),
        });
        let compound_clip =
            Clip::from_source_range(ClipId(2), ClipSource::Media(compound_media), 0, 10, 5, vv_core::Rational::one());
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![compound_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();

        let before = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (2, 2)).unwrap();
        assert_eq!(before, solid_i420(2, 2, BLACK_I420), "prima della compound clip: vuoto");

        let during = render_video_frame(&project, &tl, &compositor, &mut provider, 7, (2, 2)).unwrap();
        assert_eq!(during, solid_i420(2, 2, RED_I420), "dentro: il contenuto della timeline annidata");
    }

    /// Bug reported by the user: a compound clip is a timeline like any
    /// other, so where its nested timeline has nothing to show it must
    /// stay transparent and let the track below show through —
    /// not cover it with black.
    #[test]
    fn render_video_frame_lets_the_track_below_show_through_the_compound_clips_empty_area() {
        let mut project = Project::default();
        let mut nested_clip = solid_color_clip(1, 0, 10, red());
        // Cuts away the right half (crop in timeline pixels, nested 4x2).
        nested_clip.effects.transform = vv_core::TransformTracks::constant(vv_core::Transform {
            crop: [0.0, 0.0, 2.0, 0.0],
            ..Default::default()
        });
        let nested_id = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (4, 2),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![nested_clip],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            }],
        });
        let compound_media = project.media_pool.insert(vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 10,
                fps: vv_core::Rational::new(25, 1),
                width: 4,
                height: 2,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(nested_id),
        });
        let compound_clip =
            Clip::from_source_range(ClipId(2), ClipSource::Media(compound_media), 0, 10, 0, vv_core::Rational::one());
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(3, 0, 10, blue())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![compound_clip],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();

        let frame = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (4, 2)).unwrap();
        // Y plane, one byte per pixel: left covered by the red of the
        // compound clip, right uncovered (the blue below must show).
        assert_eq!(frame[0], RED_I420[0], "sinistra: il rosso della compound clip");
        assert_eq!(frame[3], BLUE_I420[0], "destra: il blu della track sotto, non nero");
    }

    /// A PNG with transparency imported into the pool must let the track
    /// below show through where it is transparent, not cover it: its alpha must
    /// be preserved from decode all the way to compositing.
    #[test]
    fn render_video_frame_lets_the_track_below_show_through_a_transparent_png() {
        let dir = std::env::temp_dir().join("vv-app-export-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("half_transparent.png");
        // Left half opaque red, right half fully transparent.
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=4x2:d=1",
                "-vf",
                "format=rgba,geq=r='255':g='0':b='0':a='if(lt(X,2),255,0)'",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );

        let mut project = Project::default();
        let meta = vv_media::probe::probe_image(&path).expect("probe della PNG fallito");
        let png_media = project.media_pool.insert(vv_core::MediaItem {
            path: path.clone(),
            meta,
            content_hash: 1,
            compound: None,
        });
        let png_clip =
            Clip::from_source_range(ClipId(2), ClipSource::Media(png_media), 0, 10, 0, vv_core::Rational::one());
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(3, 0, 10, blue())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![png_clip],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();

        let frame = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (4, 2)).unwrap();
        assert!(
            (frame[0] as i16 - RED_I420[0] as i16).abs() <= 4,
            "sinistra: il rosso opaco della PNG, non il blu ({})",
            frame[0]
        );
        assert!(
            (frame[3] as i16 - BLUE_I420[0] as i16).abs() <= 4,
            "destra: trasparente, si deve vedere il blu sotto ({})",
            frame[3]
        );
    }

    #[test]
    fn render_video_frame_applies_the_transform_to_a_solid_color_clip() {
        let project = Project::default();
        let mut clip = solid_color_clip(1, 0, 5, red());
        // Crop in timeline pixels (4x2): away with the right half.
        clip.effects.transform = vv_core::TransformTracks::constant(vv_core::Transform {
            crop: [0.0, 0.0, 2.0, 0.0],
            ..Default::default()
        });
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame =
            render_video_frame(&project, &tl, &compositor, &mut active, 0, (4, 2)).unwrap();
        // First row of the Y plane.
        assert_eq!(frame[0], RED_I420[0]);
        assert_eq!(frame[3], BLACK_I420[0]);
    }

    /// A media missing from the pool is an `Err`, not a black frame.
    #[test]
    fn render_video_frame_fails_loudly_when_the_clip_references_a_missing_media() {
        let missing_media_id = {
            let mut other_project = Project::default();
            other_project.media_pool.insert(vv_core::MediaItem {
                path: "dummy.mp4".into(),
                meta: vv_core::MediaMeta {
                    duration_frames: 0,
                    fps: vv_core::Rational::new(25, 1),
                    width: 0,
                    height: 0,
                    has_video: true,
                    has_audio: false,
                    sample_rate: 0,
                    channels: 0,
                    audio_streams: 0,
                },
                content_hash: 0,
                compound: None,
            })
        };
        let project = Project::default();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![Clip::from_source_range(
                ClipId(1),
                ClipSource::Media(missing_media_id),
                0,
                10,
                0,
                vv_core::Rational::one(),
            )],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();
        let err = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (2, 2))
            .expect_err("un media assente dal pool deve fallire, non produrre un frame nero");
        assert!(err.contains("media not found"), "err={err}");
    }

    /// Where the top track has no clip, the one below shows.
    #[test]
    fn render_video_frame_prefers_the_topmost_video_track() {
        let project = Project::default();
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 30, red())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(2, 10, 10, blue())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();

        let below =
            render_video_frame(&project, &tl, &compositor, &mut provider, 5, (2, 2)).unwrap();
        assert_eq!(below, solid_i420(2, 2, RED_I420), "sotto la track top: si vede quella bottom");

        let above =
            render_video_frame(&project, &tl, &compositor, &mut provider, 15, (2, 2)).unwrap();
        assert_eq!(above, solid_i420(2, 2, BLUE_I420), "la track top ha una clip qui: vince lei");
    }

    #[test]
    fn mix_audio_track_is_silence_when_no_audio_track_has_clips() {
        let project = Project::default();
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 25, red())],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);
        let mixed = mix_audio_track(&project, &tl, 0..25).unwrap();
        assert_eq!(
            mixed.len(),
            (PROJECT_SAMPLE_RATE as usize) * PROJECT_CHANNELS as usize
        );
        assert!(mixed.iter().all(|&s| s == 0.0));
    }

    /// A real audio file inside the nested timeline of a compound clip
    /// must reach the export mix, at the position of the compound
    /// clip in the outer timeline — not at the one it would have in its
    /// nested timeline.
    #[test]
    fn mix_audio_track_recurses_into_a_compound_clips_nested_timeline() {
        let dir = std::env::temp_dir().join("vv-app-export-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compound_audio_source.wav");
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
        let real_clip =
            Clip::from_source_range(ClipId(100), ClipSource::Media(real_media), 0, 25, 0, vv_core::Rational::one());
        let nested_id = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1, 1),
            tracks: vec![Track {
                kind: TrackKind::Audio,
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
        // At 25 (1s after the start): silence before, sine wave during.
        let compound_clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(compound_media), 0, 25, 25, vv_core::Rational::one());
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Audio,
            clips: vec![compound_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let mixed = mix_audio_track(&project, &tl, 0..50).unwrap();
        let per_second = PROJECT_SAMPLE_RATE as usize * PROJECT_CHANNELS as usize;
        assert!(mixed[..per_second].iter().all(|&s| s == 0.0), "silenzio prima della compound clip");
        assert!(
            mixed[per_second..].iter().any(|&s| s.abs() > 0.01),
            "il contenuto della timeline annidata arriva nel mix alla posizione della compound clip"
        );
    }

    /// End-to-end: builds a real timeline (via `VenturiApp`, not
    /// `Clip`/`Track` by hand) with a real video+audio file generated by
    /// the ffmpeg CLI (same pattern as the tests in `main.rs`), exports, and
    /// checks the result by reading it back with `vv_media` — the only way to
    /// verify the export in this environment, without a display.
    #[test]
    fn export_timeline_produces_a_playable_file_matching_the_timeline() {
        let dir = std::env::temp_dir().join("vv-app-export-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ],
            &source_path,
        );

        let mut app = crate::VenturiApp::default();
        app.import_media(source_path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("media importato atteso nel pool");
        app.add_media_to_timeline(media_id);

        let output_path = dir.join("out.mp4");
        let progress = Mutex::new(ExportProgress::default());
        let cancel = AtomicBool::new(false);
        let total = app.project.timelines[timeline_id].total_frames();
        let settings = ExportSettings::new(output_path.clone());
        export_timeline(&app.project, timeline_id, &settings, 0..total, &progress, &cancel)
            .expect("export fallito");

        assert!(progress.lock().unwrap().done);

        let meta = vv_media::probe(&output_path).unwrap();
        assert_eq!(meta.width, 64);
        assert_eq!(meta.height, 48);
        assert!(meta.has_audio);

        let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
        let mut count = 0;
        while decoder.next_frame().unwrap().is_some() {
            count += 1;
        }
        // ~25fps for 1s, the same tolerance already used for the B-frame
        // encoder reordering seen in the `vv_media::encode` tests.
        assert!((24..=25).contains(&count), "count={count}");

        let audio = vv_media::decode_audio_track(&output_path, 0)
            .unwrap()
            .expect("audio atteso nell'export");
        let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }

    /// Regression for a user report of audio running ahead of the
    /// video on export (perceived with mpv, 2-3 frames): a black->white
    /// flash (`ClipSource::SolidColor`, no video encode/decode
    /// involved in its position) and a beep (a real audio clip) that
    /// start at the same timeline frame, at a fractional NTSC fps
    /// (30000/1001) on purpose so that `PROJECT_SAMPLE_RATE / fps` is not an
    /// integer (at 24fps it is exactly 2000, which would hide a
    /// rounding bug) — the real case isolated from the report
    /// was already at an integer fps, and here it comes out perfectly in
    /// sync anyway: if this test breaks, the bug is in the
    /// frame<->sample rounding of `export.rs`/`encode.rs`/`vv_audio::mixer`, not
    /// in the original cause of the report (probably on the
    /// player side, not in this export).
    #[test]
    fn export_keeps_video_and_audio_frame_accurate_at_a_fractional_ntsc_fps() {
        let dir = std::env::temp_dir().join("vv-app-diag-avsync-frac");
        std::fs::create_dir_all(&dir).unwrap();
        let beep_path = dir.join("beep.wav");
        vv_media::test_support::ffmpeg(
            &["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=1"],
            &beep_path,
        );

        let mut project = Project::default();
        let beep_media = project.media_pool.insert(vv_core::MediaItem {
            path: beep_path,
            meta: vv_core::MediaMeta {
                duration_frames: 30,
                fps: vv_core::Rational::new(30000, 1001),
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
        const FLASH_FRAME: FrameIdx = 20;
        let beep_clip = Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(beep_media),
            0,
            25,
            FLASH_FRAME,
            vv_core::Rational::one(),
        );
        let tl = Timeline {
            name: "diag".into(),
            fps: vv_core::Rational::new(30000, 1001),
            resolution: (64, 48),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![
                        solid_color_clip(10, 0, FLASH_FRAME, black()),
                        solid_color_clip(11, FLASH_FRAME, 15, white()),
                    ],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![beep_clip],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
            ],
        };
        let timeline_id = project.timelines.insert(tl);

        let output_path = dir.join("out.mp4");
        let progress = Mutex::new(ExportProgress::default());
        let cancel = AtomicBool::new(false);
        let total = project.timelines[timeline_id].total_frames();
        let settings = ExportSettings::new(output_path.clone());
        export_timeline(&project, timeline_id, &settings, 0..total, &progress, &cancel)
            .expect("export fallito");

        let fps = 30000.0_f64 / 1001.0;

        let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
        let mut white_frame_idx = None;
        let mut idx = 0i64;
        while let Some((_, frame)) = decoder.next_frame().unwrap() {
            let avg_y = frame.y.iter().map(|&b| b as u64).sum::<u64>() / frame.y.len() as u64;
            if avg_y > 128 && white_frame_idx.is_none() {
                white_frame_idx = Some(idx);
            }
            idx += 1;
        }
        let white_frame_idx = white_frame_idx.expect("nessun frame bianco trovato nell'export");

        let audio = vv_media::decode_audio_track(&output_path, 0).unwrap().expect("audio atteso");
        let mut onset_sample = None;
        for (i, &s) in audio.samples.iter().enumerate() {
            if s.abs() > 0.05 {
                onset_sample = Some(i as u64 / audio.channels as u64);
                break;
            }
        }
        let onset_sample = onset_sample.expect("nessun onset audio trovato nell'export");
        let onset_secs = onset_sample as f64 / audio.sample_rate as f64;
        let onset_frame = (onset_secs * fps).floor() as i64;

        assert_eq!(white_frame_idx, FLASH_FRAME, "il flash video non è al frame atteso");
        assert_eq!(
            onset_frame, FLASH_FRAME,
            "l'attacco audio ({onset_secs:.6}s) cade nel frame {onset_frame} invece del frame {FLASH_FRAME} del flash video: sfasamento di {} frame",
            FLASH_FRAME - onset_frame
        );
    }

    fn black() -> Rgba {
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }
    }

    fn white() -> Rgba {
        Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
    }

    /// End-to-end regression for exporting an image (see
    /// `ActiveClipDecoder::open_for`): the default clip (5s = 125
    /// frames at 25fps) covers well past the single real frame
    /// an image has — before the dedicated support the export would
    /// have errored out (or stopped) as soon as it went past the first
    /// requested position.
    #[test]
    fn export_timeline_covers_a_stretched_image_clip_past_its_only_real_frame() {
        let dir = std::env::temp_dir().join("vv-app-export-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("still.png");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=yellow:size=64x48:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &source_path,
        );

        let mut app = crate::VenturiApp::default();
        app.import_media(source_path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound.is_none())
            .map(|(id, _)| id)
            .expect("media importato atteso nel pool");
        assert!(app.project.media_pool[media_id].meta.is_image());
        app.add_media_to_timeline(media_id);

        let output_path = dir.join("out.mp4");
        let progress = Mutex::new(ExportProgress::default());
        let cancel = AtomicBool::new(false);
        let total = app.project.timelines[timeline_id].total_frames();
        assert_eq!(total, 5 * 25, "5 s di default a 25 fps");
        let settings = ExportSettings::new(output_path.clone());
        export_timeline(&app.project, timeline_id, &settings, 0..total, &progress, &cancel)
            .expect("export fallito");

        assert!(progress.lock().unwrap().done);

        let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
        let mut count = 0;
        while let Some((_, frame)) = decoder.next_frame().unwrap() {
            // Yellow over the whole frame, for the whole export: if
            // the image "ended" halfway, black would appear here (or an
            // error would already have interrupted the export above).
            assert!(frame.y[0] > 150, "atteso ancora il frame dell'immagine, non nero");
            count += 1;
        }
        assert!((total - 1..=total).contains(&count), "count={count}");
    }

    /// The bug case, end-to-end: a clip at 23.976 fps appended onto a
    /// timeline at 25 (created from the first media, at 25). Conformed, the
    /// second clip occupies its real time on the timeline, so the exported
    /// file lasts as long as the two clips together and the audio of the second
    /// reaches the very end instead of finishing before the video.
    #[test]
    fn export_conforms_a_clip_whose_fps_differs_from_the_timeline() {
        let dir = std::env::temp_dir().join("vv-app-export-conform-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mute_25 = dir.join("mute25.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=sample_rate=48000:channel_layout=stereo",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ],
            &mute_25,
        );

        // 24000/1001 = 23.976 fps: 48 source frames for 2 real s.
        let sine_23976 = dir.join("sine23976.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=24000/1001:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ],
            &sine_23976,
        );

        let mut app = crate::VenturiApp::default();
        app.import_media(mute_25);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        assert_eq!(
            app.project.timelines[timeline_id].fps,
            vv_core::Rational::new(25, 1)
        );
        let first = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).map(|(id, _)| id).unwrap();
        app.add_media_to_timeline(first);

        app.import_media(sine_23976);
        let second = app
            .project
            .media_pool
            .iter()
            .filter(|(_, item)| item.compound.is_none())
            .map(|(id, _)| id)
            .find(|id| *id != first)
            .expect("secondo media atteso nel pool");
        let second_start = app.project.timelines[timeline_id].total_frames();
        app.add_media_to_timeline(second);

        let conformed = app.project.timelines[timeline_id]
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .find(|c| c.timeline_start == second_start)
            .expect("clip conformata attesa");
        assert_ne!(conformed.rate, vv_core::Rational::one());
        let total = app.project.timelines[timeline_id].total_frames();
        // 1 s at 25 fps + 2 s conformed to 25 fps, within one frame of
        // rounding on the duration reported by ffmpeg.
        assert!((74..=76).contains(&total), "total={total}");

        let output_path = dir.join("out.mp4");
        let progress = Mutex::new(ExportProgress::default());
        export_timeline(
            &app.project,
            timeline_id,
            &ExportSettings::new(output_path.clone()),
            0..total,
            &progress,
            &AtomicBool::new(false),
        )
        .expect("export fallito");

        let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
        let mut count = 0;
        while decoder.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert!((total - 1..=total).contains(&count), "count={count}");

        let audio = vv_media::decode_audio_track(&output_path, 0)
            .unwrap()
            .expect("audio atteso nell'export");
        let frames = audio.samples.len() / audio.channels as usize;
        let secs = frames as f64 / audio.sample_rate as f64;
        let expected_secs = total as f64 / 25.0;
        assert!(
            (secs - expected_secs).abs() < 0.1,
            "audio {secs}s contro {expected_secs}s di video"
        );

        // The audio of the second clip covers its stretch to the end:
        // if the video were mapped 1:1 onto the source frames, the timeline
        // would end earlier and the tail of the sine would be cut off.
        let peak_in = |from_secs: f64, to_secs: f64| {
            let ch = audio.channels as usize;
            let from = (from_secs * audio.sample_rate as f64) as usize * ch;
            let to = ((to_secs * audio.sample_rate as f64) as usize * ch).min(audio.samples.len());
            audio.samples[from.min(to)..to]
                .iter()
                .fold(0.0_f32, |m, s| m.max(s.abs()))
        };
        assert!(peak_in(0.1, 0.9) < 0.05, "la prima clip è muta");
        assert!(peak_in(1.1, 1.9) > 0.1, "la seconda clip suona");
        assert!(
            peak_in(expected_secs - 0.2, expected_secs) > 0.1,
            "il sine deve arrivare fino alla fine della timeline"
        );
    }

    #[test]
    fn preferred_settings_pick_the_faster_encoders_only_when_available() {
        let settings = ExportSettings::preferred(PathBuf::from("out.mp4"));
        let nvenc = vv_media::VideoCodec::Nvenc.is_available();
        assert_eq!(settings.video.codec == vv_media::VideoCodec::Nvenc, nvenc);
        assert_eq!(settings.video.preset, settings.video.codec.default_preset());
        let fdk = vv_media::AudioCodec::FdkAac.is_available();
        let audio = settings.audio.expect("audio incluso di default");
        assert_eq!(audio.codec == vv_media::AudioCodec::FdkAac, fdk);
    }

    #[test]
    fn export_timeline_scales_the_output_and_can_drop_the_audio() {
        let dir = std::env::temp_dir().join("vv-app-export-scale-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ],
            &source_path,
        );

        let mut app = crate::VenturiApp::default();
        app.import_media(source_path);
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).map(|(id, _)| id).unwrap();
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();
        let total = app.project.timelines[timeline_id].total_frames();

        let mut settings = ExportSettings::new(dir.join("out.mp4"));
        settings.scale_percent = 50;
        settings.audio = None;
        export_timeline(
            &app.project,
            timeline_id,
            &settings,
            0..total,
            &Mutex::new(ExportProgress::default()),
            &AtomicBool::new(false),
        )
        .expect("export fallito");

        let meta = vv_media::probe(&settings.output_path).unwrap();
        assert_eq!((meta.width, meta.height), (32, 24));
        assert!(!meta.has_audio);
    }

    #[test]
    fn export_timeline_writes_only_the_in_out_range() {
        let dir = std::env::temp_dir().join("vv-app-export-range-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &source_path,
        );

        let mut app = crate::VenturiApp::default();
        app.import_media(source_path);
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).map(|(id, _)| id).unwrap();
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();

        let output_path = dir.join("out.mp4");
        let progress = Mutex::new(ExportProgress::default());
        export_timeline(
            &app.project,
            timeline_id,
            &ExportSettings::new(output_path.clone()),
            5..15,
            &progress,
            &AtomicBool::new(false),
        )
        .expect("export fallito");
        assert_eq!(progress.lock().unwrap().total_frames, 10);

        let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
        let mut count = 0;
        while decoder.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert!((9..=10).contains(&count), "count={count}");
    }
}
