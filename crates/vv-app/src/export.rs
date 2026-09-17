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
//! bottom->top, la più in alto vince dove ha una clip — nessuna opacità
//! per-clip ancora, quindi "vince" invece di un vero accumulo alpha, vedi
//! `Timeline::active_video_clip_at`) e N track audio (sommate,
//! REFACTOR_PIPELINE.md B4); `EffectStack::speed` non applicato (nessun
//! time-remap: milestone 7 non ancora fatta).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vv_core::{
    Clip, ClipId, ClipSource, FrameIdx, Project, Rgba, Timeline, TimelineId, TrackKind,
};

use vv_audio::mixer::{
    MixSnapshot, PROJECT_SAMPLE_RATE, mix_range, prepare_mix_buffer, timeline_frame_to_sample,
};

use crate::frame_provider::{FrameProvider, as_render_yuv_frame, media_source_frame};

const PROJECT_CHANNELS: u16 = 2;

#[derive(Default)]
pub struct ExportProgress {
    pub current_frame: FrameIdx,
    pub total_frames: FrameIdx,
    pub done: bool,
    pub error: Option<String>,
}

fn black_frame(resolution: (u32, u32)) -> Vec<u8> {
    vv_render::solid_color_frame(
        Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        },
        resolution.0,
        resolution.1,
    )
}

/// Decoder tenuto aperto per la clip video attiva, con seek/riapertura solo
/// quando la clip cambia (non un decoder nuovo per ogni frame di output:
/// ogni seek è un flush a keyframe, troppo lento fatto ad ogni frame).
struct ActiveClipDecoder {
    clip_id: ClipId,
    decoder: vv_media::Decoder,
}

impl ActiveClipDecoder {
    fn open_for(
        clip_id: ClipId,
        path: &Path,
        target_source_frame: FrameIdx,
    ) -> Result<Self, String> {
        let mut decoder = vv_media::Decoder::open(path).map_err(|e| e.to_string())?;
        let secs = target_source_frame as f64 / decoder.fps().as_f64().max(1e-9);
        decoder.seek_to_time(secs).map_err(|e| e.to_string())?;
        let mut me = Self { clip_id, decoder };
        me.advance_to(target_source_frame)?;
        Ok(me)
    }

    /// Decodifica in avanti fino a raggiungere (o superare) `target`,
    /// scartando i frame intermedi. `None` a fine stream (capita se
    /// `source_out` va oltre la fine reale del file).
    fn advance_to(&mut self, target: FrameIdx) -> Result<Option<vv_media::FrameYuv420>, String> {
        loop {
            match self.decoder.next_frame().map_err(|e| e.to_string())? {
                Some((idx, frame)) if idx >= target => return Ok(Some(frame)),
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
    active: Option<ActiveClipDecoder>,
}

impl FrameProvider for StreamingFrameProvider {
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<vv_media::FrameYuv420>>, String> {
        let Some((media_id, source_frame)) = media_source_frame(clip, timeline_frame) else {
            self.active = None;
            return Ok(None);
        };
        let path = project
            .media_pool
            .get(media_id)
            .ok_or_else(|| "media non trovato nel pool".to_string())?
            .path
            .clone();

        if self.active.as_ref().is_none_or(|a| a.clip_id != clip.id) {
            self.active = Some(ActiveClipDecoder::open_for(clip.id, &path, source_frame)?);
        }
        let frame = self
            .active
            .as_mut()
            .expect("appena assegnato sopra se assente")
            .advance_to(source_frame)?;
        Ok(frame.map(Arc::new))
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
    output_path: &Path,
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

    let has_audio_track = timeline
        .tracks_of_kind(TrackKind::Audio)
        .any(|(_, t)| !t.clips.is_empty());

    let mut encoder = vv_media::Encoder::new(
        output_path,
        timeline.resolution.0,
        timeline.resolution.1,
        timeline.fps,
        has_audio_track.then_some((PROJECT_SAMPLE_RATE, PROJECT_CHANNELS)),
    )
    .map_err(|e| e.to_string())?;

    // Compositor indipendente, di proprietà di questo thread: evita
    // qualunque contesa GPU col device della UI (che ha il suo,
    // `Compositor::new_headless()` in `main.rs`).
    let compositor = vv_render::Compositor::new_headless();

    let mut provider = StreamingFrameProvider::default();
    for frame in range.clone() {
        if cancel.load(Ordering::Relaxed) {
            return Err("annullato".to_string());
        }

        let rgba = render_video_frame(
            project,
            timeline,
            &compositor,
            &mut provider,
            frame,
            timeline.resolution,
        )?;
        encoder
            .write_video_frame(&rgba)
            .map_err(|e| e.to_string())?;

        progress.lock().unwrap().current_frame = frame - range.start + 1;
    }

    if has_audio_track {
        let mixed = mix_audio_track(project, timeline, range)?;
        encoder
            .write_audio_samples(&mixed)
            .map_err(|e| e.to_string())?;
    }

    encoder.finish().map_err(|e| e.to_string())?;
    progress.lock().unwrap().done = true;
    Ok(())
}

fn render_video_frame(
    project: &Project,
    timeline: &Timeline,
    compositor: &vv_render::Compositor,
    provider: &mut StreamingFrameProvider,
    frame: FrameIdx,
    resolution: (u32, u32),
) -> Result<Vec<u8>, String> {
    let Some((_, clip)) = timeline.active_video_clip_at(frame) else {
        provider.active = None;
        return Ok(black_frame(resolution));
    };

    match &clip.source {
        ClipSource::Media(_) => {
            // Mappatura clip→frame-sorgente condivisa con l'anteprima
            // via `FrameProvider` (REFACTOR_PIPELINE.md B1) — solo il
            // transform la ricalcola qui perché serve indipendentemente
            // da `provider` avere restituito un frame o `None`.
            let transform = clip.effects.transform.value_at(clip.source_frame_at(frame));
            Ok(match provider.frame_for(project, clip, frame)? {
                Some(f) => compositor.render_frame(
                    &as_render_yuv_frame(&f),
                    &transform,
                    resolution.0,
                    resolution.1,
                ),
                // source_out oltre la fine reale del file decodificato:
                // meglio un frame nero che un panic o il congelamento
                // dell'ultimo frame valido.
                None => black_frame(resolution),
            })
        }
        ClipSource::SolidColor => {
            provider.active = None;
            let local = frame - clip.timeline_start;
            let color = clip
                .effects
                .color
                .as_ref()
                .map(|k| k.value_at(local))
                .unwrap_or(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                });
            Ok(vv_render::solid_color_frame(
                color,
                resolution.0,
                resolution.1,
            ))
        }
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
    for (_, track) in timeline.tracks_of_kind(TrackKind::Audio) {
        for clip in &track.clips {
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
    use vv_core::{Clip, EffectStack, Keyframed, Track, TrackKind};

    fn solid_color_clip(id: u64, start: FrameIdx, len: FrameIdx, color: Rgba) -> Clip {
        Clip {
            id: ClipId(id),
            source: ClipSource::SolidColor,
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: EffectStack {
                color: Some(Keyframed::constant(color)),
                ..EffectStack::default()
            },
            linked_group: None,
            audio_stream_index: 0,
        }
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
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![solid_color_clip(2, 5, 20, red())], // finisce a 25, più avanti
                muted: false,
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
        }]);
        assert_eq!(tl.active_clip_at(0, 5).map(|c| c.id), Some(ClipId(1)));
        assert!(tl.active_clip_at(0, 15).is_none(), "buco tra le due clip");
        assert_eq!(tl.active_clip_at(0, 25).map(|c| c.id), Some(ClipId(2)));
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
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame = render_video_frame(&project, &tl, &compositor, &mut active, 0, (2, 2)).unwrap();
        assert!(
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px == &[0, 0, 0, 255])
        );
    }

    #[test]
    fn render_video_frame_reads_solid_color_at_the_clips_local_frame() {
        let project = Project::default();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 10, 5, red())],
            muted: false,
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = StreamingFrameProvider::default();
        let frame =
            render_video_frame(&project, &tl, &compositor, &mut active, 12, (2, 2)).unwrap();
        assert!(
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px == &[255, 0, 0, 255])
        );
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
            clips: vec![Clip {
                id: ClipId(1),
                source: ClipSource::Media(missing_media_id),
                source_in: 0,
                source_out: 10,
                timeline_start: 0,
                effects: EffectStack::default(),
                linked_group: None,
                audio_stream_index: 0,
            }],
            muted: false,
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
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(2, 10, 10, blue())],
                muted: false,
            },
        ]);
        let compositor = vv_render::Compositor::new_headless();
        let mut provider = StreamingFrameProvider::default();

        let below =
            render_video_frame(&project, &tl, &compositor, &mut provider, 5, (2, 2)).unwrap();
        assert!(
            below
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px == &[255, 0, 0, 255]),
            "sotto la track top: si vede quella bottom"
        );

        let above =
            render_video_frame(&project, &tl, &compositor, &mut provider, 15, (2, 2)).unwrap();
        assert!(
            above
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px == &[0, 0, 255, 255]),
            "la track top ha una clip qui: vince lei"
        );
    }

    #[test]
    fn mix_audio_track_is_silence_when_no_audio_track_has_clips() {
        let project = Project::default();
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![solid_color_clip(1, 0, 25, red())],
                muted: false,
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![],
                muted: false,
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
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![],
                muted: false,
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
        export_timeline(&app.project, timeline_id, &output_path, 0..total, &progress, &cancel)
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
            &output_path,
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
