//! Pipeline di export (milestone 9): cammina l'intera timeline e produce un
//! file H.264+AAC (`vv_media::Encoder`), applicando lo stesso stack effetti
//! (crop/zoom/gain keyframeati, colore per le clip SolidColor) già valutato
//! dall'anteprima in `main.rs` (`active_clip_effects`,
//! `EffectStack::*.value_at`). Nessun audio device, nessuna finestra: pensata
//! per girare su un thread dedicato a partire da uno snapshot di `Project`
//! clonato al click di "Esporta" (vedi `VibeVideoApp::start_export` in `main.rs`),
//! non sul `Project` live della UI.
//!
//! Limiti v1, coerenti con lo stato attuale del progetto (ARCHITECTURE.md):
//! N track video (compositate
//! bottom->top in alpha-over, `Timeline::active_video_clips_at` — nessuna
//! opacità per-clip ancora, quindi un layer che copre tutto il frame
//! occlude quelli sotto) e N track audio (sommate,
//! REFACTOR_PIPELINE.md B4); `EffectStack::speed` non applicato (nessun
//! time-remap: milestone 7 non ancora fatta).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vv_core::{
    Clip, ClipId, ClipSource, FrameIdx, Project, Rgba, Timeline, TimelineId, TrackKind,
};

use vv_audio::mixer::{
    MixSnapshot, PROJECT_SAMPLE_RATE, mix_range, prepare_mix_buffer, timeline_frame_to_sample,
};

use crate::frame_provider::{
    FrameProvider, as_render_yuv_frame, clip_source_size, media_source_frame,
};

const PROJECT_CHANNELS: u16 = 2;
const RENDER_AHEAD_FRAMES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportSettings {
    pub output_path: PathBuf,
    /// Percentuale della risoluzione della timeline.
    pub scale_percent: u32,
    pub video: vv_media::VideoSettings,
    /// `None` = export senza audio.
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

    /// Come `new`, ma con gli encoder più veloci disponibili su questa
    /// macchina (NVENC, FDK). `new` resta deterministico per i test.
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

    /// Ridotta a dimensioni pari (richiesto dal 4:2:0 degli encoder);
    /// al 100% resta esattamente quella della timeline.
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
}

/// Decoder tenuto aperto per la clip video attiva, con seek/riapertura solo
/// quando la clip cambia (non un decoder nuovo per ogni frame di output:
/// ogni seek è un flush a keyframe, troppo lento fatto ad ogni frame).
struct ActiveClipDecoder {
    decoder: vv_media::Decoder,
    /// Ultimo frame decodificato, tenuto per poterlo restituire di nuovo:
    /// una clip conformata (`Clip::rate`) chiede lo stesso frame sorgente
    /// su due frame di timeline consecutivi, e il decoder non sa tornare
    /// indietro di uno.
    last: Option<(FrameIdx, Arc<vv_media::FrameYuv420>)>,
}

impl ActiveClipDecoder {
    fn open_for(path: &Path, target_source_frame: FrameIdx, is_image: bool) -> Result<Self, String> {
        // Un'immagine ferma va aperta con `open_image` (vedi la sua doc):
        // un vero `Decoder::open` si comporterebbe come un video a un
        // frame solo, andando in EOF (quindi `advance_to` restituirebbe
        // `None`, un frame nero) oltre la primissima posizione chiesta.
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

    /// Decodifica in avanti fino a raggiungere (o superare) `target`,
    /// scartando i frame intermedi; un `target` già raggiunto restituisce
    /// di nuovo l'ultimo frame invece di avanzare. `None` a fine stream
    /// (capita se `source_out` va oltre la fine reale del file).
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

/// Implementazione di `FrameProvider` per l'export: streaming sincrono,
/// un `ActiveClipDecoder` tenuto aperto per la clip Media attiva
/// (riaperto solo al cambio di clip — mai un decoder nuovo per ogni
/// frame). A differenza della cache dell'anteprima, un errore di
/// decodifica qui è un vero `Err`, non un `Ok(None)`: l'export non deve
/// mai trasformare in silenzio un file che non si apre in un frame nero
/// (REFACTOR_PIPELINE.md B1, doc di `FrameProvider`).
#[derive(Default)]
struct StreamingFrameProvider {
    /// Un decoder per clip, non uno solo: a un dato frame di timeline
    /// possono essere attive più track video sovrapposte (vedi
    /// `Timeline::active_video_clips_at`) e tenerne aperto uno solo
    /// significherebbe riaprire e riseekare tutti gli altri a ogni frame.
    /// Potato da `retain_clips` appena una clip esce di scena.
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
            .ok_or_else(|| "media non trovato nel pool".to_string())?;
        let path = item.path.clone();
        let is_image = item.meta.is_image();

        let decoder = match self.active.entry(clip.id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(ActiveClipDecoder::open_for(&path, source_frame, is_image)?),
        };
        decoder.advance_to(source_frame)
    }
}

/// Cammina i frame `range` della timeline (l'intervallo in/out) e produce
/// `output_path`. Bloccante: il chiamante (`main.rs`) lo gira su un thread
/// dedicato. `project` è uno snapshot clonato al momento del click, non
/// condiviso con la UI — modificare il progetto durante l'export non lo
/// tocca.
pub fn export_timeline(
    project: &Project,
    timeline_id: TimelineId,
    settings: &ExportSettings,
    range: std::ops::Range<FrameIdx>,
    progress: &Mutex<ExportProgress>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let timeline = project
        .timelines
        .get(timeline_id)
        .ok_or_else(|| "timeline non trovata".to_string())?;

    let range = range.start.max(0)..range.end.min(timeline.total_frames());
    let total_frames = (range.end - range.start).max(0);
    progress.lock().unwrap().total_frames = total_frames;
    if total_frames <= 0 {
        progress.lock().unwrap().done = true;
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

    // Decode, composizione GPU ed encode su tre thread: in serie ognuno
    // aspettava gli altri e nessuno saturava la macchina.
    let (decoded_tx, decoded_rx) =
        std::sync::mpsc::sync_channel::<Result<DecodedLayers, String>>(RENDER_AHEAD_FRAMES);
    let (composed_tx, composed_rx) =
        std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(RENDER_AHEAD_FRAMES);
    let resolution = timeline.resolution;
    let output = vv_render::OutputFrame::scaled(out_w, out_h, resolution);
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
                    decode_video_frame(project, timeline, &mut provider, frame, resolution);
                let failed = decoded.is_err();
                // `send` fallisce solo se lo stadio dopo ha già smesso.
                if decoded_tx.send(decoded).is_err() || failed {
                    return;
                }
            }
        });

        let compose_range = range.clone();
        scope.spawn(move || {
            // Compositor indipendente: evita qualunque contesa GPU col
            // device della UI (che ha il suo, in `main.rs`).
            let compositor = vv_render::Compositor::new_headless();
            for (frame, decoded) in compose_range.zip(decoded_rx) {
                let composed = decoded.map(|decoded| {
                    compose_video_frame(timeline, &compositor, frame, &decoded, output)
                });
                let failed = composed.is_err();
                if composed_tx.send(composed).is_err() || failed {
                    return;
                }
            }
        });

        // Dentro la closure: se l'encoder esce in errore `composed_rx` va
        // chiuso prima del join, o gli stadi a monte restano su `send`.
        let composed_rx = composed_rx;
        // L'audio va scritto man mano col video: tutto in coda al video, il
        // muxer si tiene in RAM l'intero video e l'interleave diventa
        // quadratico (minuti su una timeline di pochi minuti).
        let mut audio = AudioInterleaver::new(timeline, range.start);
        for frame in range.clone() {
            if cancel.load(Ordering::Relaxed) {
                return Err("annullato".to_string());
            }
            let frame_i420 = match composed_rx.recv() {
                Ok(frame_i420) => frame_i420?,
                Err(_) => return Err("annullato".to_string()),
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
    progress.lock().unwrap().done = true;
    Ok(())
}

/// Per ogni clip attiva (stesso ordine di `active_video_clips_at`): il
/// frame decodificato e, per le clip Media, transform e dimensione nativa.
type DecodedLayers = Vec<(
    Option<Arc<vv_media::FrameYuv420>>,
    Option<(vv_core::Transform, (u32, u32))>,
)>;

fn active_clips(timeline: &Timeline, frame: FrameIdx) -> Vec<&Clip> {
    timeline
        .active_video_clips_at(frame)
        .into_iter()
        .map(|(_, c)| c)
        .collect()
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
    let decoded = decode_video_frame(project, timeline, provider, frame, resolution)?;
    let output = vv_render::OutputFrame::exact(resolution.0, resolution.1);
    Ok(compose_video_frame(timeline, compositor, frame, &decoded, output))
}

fn decode_video_frame(
    project: &Project,
    timeline: &Timeline,
    provider: &mut StreamingFrameProvider,
    frame: FrameIdx,
    resolution: (u32, u32),
) -> Result<DecodedLayers, String> {
    let clips = active_clips(timeline, frame);
    provider.retain_clips(&clips.iter().map(|c| c.id).collect::<Vec<_>>());

    let mut decoded = Vec::with_capacity(clips.len());
    for clip in &clips {
        match &clip.source {
            ClipSource::Media(_) => {
                // Mappatura clip→frame-sorgente condivisa con l'anteprima
                // via `FrameProvider` (REFACTOR_PIPELINE.md B1) — solo il
                // transform la ricalcola qui perché serve indipendentemente
                // da `provider` avere restituito un frame o `None`.
                let transform = clip.effects.transform.value_at(clip.source_frame_at(frame));
                // `None` = source_out oltre la fine reale del file: quel
                // layer semplicemente non c'è (nero se è l'unico), meglio
                // che un panic o il congelamento dell'ultimo frame valido.
                decoded.push((
                    provider.frame_for(project, clip, frame)?,
                    Some((transform, clip_source_size(project, clip, resolution))),
                ));
            }
            ClipSource::SolidColor | ClipSource::Text => decoded.push((None, None)),
        }
    }
    Ok(decoded)
}

fn compose_video_frame(
    timeline: &Timeline,
    compositor: &vv_render::Compositor,
    frame: FrameIdx,
    decoded: &DecodedLayers,
    output: vv_render::OutputFrame,
) -> Vec<u8> {
    let clips = active_clips(timeline, frame);
    let layers: Vec<vv_render::Layer> = clips
        .iter()
        .zip(decoded)
        .filter_map(|(clip, (frame_yuv, transform))| match (frame_yuv, transform) {
            (Some(f), Some((transform, source_size))) => Some(vv_render::Layer::Video {
                frame: as_render_yuv_frame(f),
                transform: *transform,
                source_size: *source_size,
            }),
            (None, Some(_)) => None,
            _ if matches!(clip.source, ClipSource::Text) => Some(vv_render::Layer::Text {
                title: clip.effects.title.as_ref()?,
                transform: clip.effects.transform.value_at(clip.source_frame_at(frame)),
            }),
            _ => {
                let source_frame = clip.source_frame_at(frame);
                Some(vv_render::Layer::Solid {
                    color: clip
                        .effects
                        .color
                        .as_ref()
                        .map(|k| k.value_at(source_frame))
                        .unwrap_or(Rgba {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                    transform: clip.effects.transform.value_at(source_frame),
                })
            }
        })
        .collect();

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

/// Scrive il mix audio all'encoder a pezzi, allineato ai frame video.
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

    /// Scrive i campioni fino all'inizio di `frame` (esclusivo); no-op
    /// finché il mix non è pronto.
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

/// Mix di tutte le track audio a `PROJECT_SAMPLE_RATE`/`PROJECT_CHANNELS`,
/// sui frame `range` di timeline: stessa `mix_range` dell'anteprima.
fn mix_audio_track(
    project: &Project,
    timeline: &Timeline,
    range: std::ops::Range<FrameIdx>,
) -> Result<Vec<f32>, String> {
    let mut buffers: HashMap<(PathBuf, usize), Option<Arc<Vec<f32>>>> = HashMap::new();
    for (_, track) in timeline.audible_tracks() {
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let Some(item) = project.media_pool.get(*media_id) else {
                continue;
            };
            let key = (item.path.clone(), clip.audio_stream_index);
            if buffers.contains_key(&key) {
                continue;
            }
            let buffer = vv_media::decode_audio_track(&item.path, clip.audio_stream_index)
                .map_err(|e| e.to_string())?
                .map(|audio| {
                    Arc::new(prepare_mix_buffer(
                        &audio.samples,
                        audio.sample_rate,
                        audio.channels,
                        PROJECT_SAMPLE_RATE,
                        PROJECT_CHANNELS,
                    ))
                });
            buffers.insert(key, buffer);
        }
    }

    let snapshot = MixSnapshot::from_timeline(
        project,
        timeline,
        PROJECT_SAMPLE_RATE,
        PROJECT_CHANNELS,
        |path, stream| buffers.get(&(path.to_path_buf(), stream)).cloned().flatten(),
    );
    let fps = timeline.fps.as_f64();
    let start_sample = timeline_frame_to_sample(range.start, fps, PROJECT_SAMPLE_RATE);
    let end_sample = timeline_frame_to_sample(range.end, fps, PROJECT_SAMPLE_RATE);
    let mut mixed =
        vec![0.0_f32; (end_sample - start_sample) as usize * PROJECT_CHANNELS as usize];
    mix_range(&snapshot, start_sample, &mut mixed);
    Ok(mixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{Clip, Keyframed, Track, TrackKind};

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

    #[test]
    fn timeline_total_frames_is_the_furthest_clip_end_across_tracks() {
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 10, red())],
                muted: false,
                solo: false,
                locked: false,
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![solid_color_clip(2, 5, 20, red())], // finisce a 25, più avanti
                muted: false,
                solo: false,
                locked: false,
            },
        ]);
        assert_eq!(tl.total_frames(), 25);
    }

    #[test]
    fn timeline_total_frames_is_zero_for_an_empty_timeline() {
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![],
            muted: false,
            solo: false,
            locked: false,
        }]);
        assert_eq!(tl.total_frames(), 0);
    }

    #[test]
    fn active_clip_at_finds_the_covering_clip_and_none_in_a_gap() {
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                solid_color_clip(1, 0, 10, red()),
                solid_color_clip(2, 20, 10, red()),
            ],
            muted: false,
            solo: false,
            locked: false,
        }]);
        assert_eq!(tl.active_clip_at(0, 5).map(|c| c.id), Some(ClipId(1)));
        assert!(tl.active_clip_at(0, 15).is_none(), "buco tra le due clip");
        assert_eq!(tl.active_clip_at(0, 25).map(|c| c.id), Some(ClipId(2)));
    }

    // BT.709 range limitato, come `Compositor::render_layers_i420`.
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
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame =
            render_video_frame(&project, &tl, &compositor, &mut active, 12, (2, 2)).unwrap();
        assert_eq!(frame, solid_i420(2, 2, RED_I420));
    }

    #[test]
    fn render_video_frame_applies_the_transform_to_a_solid_color_clip() {
        let project = Project::default();
        let mut clip = solid_color_clip(1, 0, 5, red());
        // Crop in pixel di timeline (4x2): via la metà destra.
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
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame =
            render_video_frame(&project, &tl, &compositor, &mut active, 0, (4, 2)).unwrap();
        // Prima riga del piano Y.
        assert_eq!(frame[0], RED_I420[0]);
        assert_eq!(frame[3], BLACK_I420[0]);
    }

    /// `FrameProvider::frame_for` (REFACTOR_PIPELINE.md B1, doc lì): un
    /// vero fallimento durante l'export deve restituire `Err`, mai
    /// scivolare in silenzio verso un `Ok` con un frame nero — quello è
    /// riservato al caso "oltre la fine reale del file", non a "il
    /// media referenziato dalla clip non esiste nel pool".
    #[test]
    fn render_video_frame_fails_loudly_when_the_clip_references_a_missing_media() {
        let missing_media_id = {
            // Un MediaId "orfano": mai inserito nel progetto usato dal
            // test, quindi `media_pool.get` restituirà `None` — esattamente
            // il caso "media non trovato nel pool" da verificare.
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
                },
                content_hash: 0,
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
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();
        let err = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (2, 2))
            .expect_err("un media assente dal pool deve fallire, non produrre un frame nero");
        assert!(err.contains("media non trovato"), "err={err}");
    }

    /// Due track video sovrapposte (REFACTOR_PIPELINE.md B4): dove la
    /// track più in alto (seconda nel vettore) non ha una clip, si vede
    /// quella sotto; dove ce l'ha, vince lei — stesso comportamento del
    /// viewer live in `main.rs`, generalizzato via
    /// `Timeline::active_video_clip_at`.
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
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(2, 10, 10, blue())],
                muted: false,
                solo: false,
                locked: false,
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
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![],
                muted: false,
                solo: false,
                locked: false,
            },
        ]);
        let mixed = mix_audio_track(&project, &tl, 0..25).unwrap();
        assert_eq!(
            mixed.len(),
            (PROJECT_SAMPLE_RATE as usize) * PROJECT_CHANNELS as usize
        );
        assert!(mixed.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn active_clip_at_ignores_other_tracks() {
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 10, red())],
                muted: false,
                solo: false,
                locked: false,
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![],
                muted: false,
                solo: false,
                locked: false,
            },
        ]);
        assert!(tl.active_clip_at(1, 5).is_none(), "track audio vuota");
    }

    /// End-to-end: costruisce una timeline vera (via `VibeVideoApp`, non
    /// `Clip`/`Track` a mano) con un vero file video+audio generato da
    /// ffmpeg CLI (stesso pattern dei test in `main.rs`), esporta, e
    /// verifica il risultato rileggendolo con `vv_media` — l'unico modo di
    /// verificare l'export in questo ambiente, senza display.
    #[test]
    fn export_timeline_produces_a_playable_file_matching_the_timeline() {
        let dir = std::env::temp_dir().join("vv-app-export-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("source.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
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
                source_path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = crate::VibeVideoApp::default();
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
        // ~25fps per 1s, stessa tolleranza già usata per il riordino
        // encoder B-frame vista nei test di `vv_media::encode`.
        assert!((24..=25).contains(&count), "count={count}");

        let audio = vv_media::decode_audio_track(&output_path, 0)
            .unwrap()
            .expect("audio atteso nell'export");
        let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }

    /// Regressione end-to-end per l'export di un'immagine (vedi
    /// `ActiveClipDecoder::open_for`): la clip di default (5s = 125
    /// frame a 25fps) copre ben oltre l'unico frame reale che
    /// un'immagine ha — prima del supporto dedicato l'export sarebbe
    /// andato in errore (o si sarebbe fermato) appena superata la prima
    /// posizione richiesta.
    #[test]
    fn export_timeline_covers_a_stretched_image_clip_past_its_only_real_frame() {
        let dir = std::env::temp_dir().join("vv-app-export-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join("still.png");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "color=c=yellow:size=64x48:rate=1:duration=1"])
            .args(["-frames:v", "1", "-update", "1", source_path.to_str().unwrap()])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = crate::VibeVideoApp::default();
        app.import_media(source_path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .next()
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
            // Gialla su tutto il fotogramma, in tutto l'export: se
            // l'immagine "finisse" a metà, qui comparirebbe nero (o un
            // errore avrebbe già interrotto l'export sopra).
            assert!(frame.y[0] > 150, "atteso ancora il frame dell'immagine, non nero");
            count += 1;
        }
        assert!((total - 1..=total).contains(&count), "count={count}");
    }

    /// Il caso del bug, end-to-end: una clip a 23,976 fps accodata su una
    /// timeline a 25 (creata dal primo media, a 25). Conformata, la
    /// seconda clip occupa in timeline il suo tempo reale, quindi il file
    /// esportato dura quanto le due clip insieme e l'audio della seconda
    /// arriva fino in fondo invece di finire prima del video.
    #[test]
    fn export_conforms_a_clip_whose_fps_differs_from_the_timeline() {
        let dir = std::env::temp_dir().join("vv-app-export-conform-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mute_25 = dir.join("mute25.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=25:duration=1"])
            .args(["-f", "lavfi", "-i", "anullsrc=sample_rate=48000:channel_layout=stereo"])
            .args(["-t", "1", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(mute_25.to_str().unwrap())
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        // 24000/1001 = 23,976 fps: 48 frame sorgente per 2 s reali.
        let sine_23976 = dir.join("sine23976.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=24000/1001:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(sine_23976.to_str().unwrap())
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = crate::VibeVideoApp::default();
        app.import_media(mute_25);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        assert_eq!(
            app.project.timelines[timeline_id].fps,
            vv_core::Rational::new(25, 1)
        );
        let first = app.project.media_pool.iter().next().map(|(id, _)| id).unwrap();
        app.add_media_to_timeline(first);

        app.import_media(sine_23976);
        let second = app
            .project
            .media_pool
            .iter()
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
        // 1 s a 25 fps + 2 s conformati a 25 fps, a meno di un frame di
        // arrotondamento sulla durata riportata da ffmpeg.
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

        // L'audio della seconda clip copre il suo tratto fino alla fine:
        // se il video fosse mappato 1:1 sui frame sorgente, la timeline
        // finirebbe prima e la coda del sine sarebbe tagliata.
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
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=25:duration=1"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(source_path.to_str().unwrap())
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = crate::VibeVideoApp::default();
        app.import_media(source_path);
        let media_id = app.project.media_pool.iter().next().map(|(id, _)| id).unwrap();
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
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=25:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(source_path.to_str().unwrap())
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut app = crate::VibeVideoApp::default();
        app.import_media(source_path);
        let media_id = app.project.media_pool.iter().next().map(|(id, _)| id).unwrap();
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
