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
        .expect("audio mix already consumed")
        .join()
        .expect("audio mix thread panicked")
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
#[path = "tests/export.rs"]
mod tests;
