//! Pipeline di export (milestone 9): cammina l'intera timeline e produce un
//! file H.264+AAC (`vv_media::Encoder`), applicando lo stesso stack effetti
//! (crop/zoom/gain keyframeati, colore per le clip SolidColor) già valutato
//! dall'anteprima in `main.rs` (`active_clip_effects`,
//! `EffectStack::*.value_at`). Nessun audio device, nessuna finestra: pensata
//! per girare su un thread dedicato a partire da uno snapshot di `Project`
//! clonato al click di "Esporta" (vedi `VibeVideoApp::export` in `main.rs`),
//! non sul `Project` live della UI.
//!
//! Limiti v1, coerenti con lo stato attuale del progetto (ARCHITECTURE.md):
//! tutta la timeline, nessuna selezione in/out; una sola track video (0) e
//! una sola track audio (1), come tutto quel che la UI può costruire oggi
//! (`VIDEO_TRACK` in `main.rs`); `EffectStack::speed` non applicato (nessun
//! time-remap: milestone 7 non ancora fatta).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use vv_core::{ClipId, ClipSource, FrameIdx, Keyframed, Project, Rgba, Timeline, TimelineId};

const VIDEO_TRACK: usize = 0;
const AUDIO_TRACK: usize = 1;
/// Sample rate/canali a cui viene mixata la traccia audio prima
/// dell'encode, indipendentemente da quelli nativi dei singoli media
/// (vedi `resample_and_remix`).
const PROJECT_SAMPLE_RATE: u32 = 48_000;
const PROJECT_CHANNELS: usize = 2;
/// Ampiezza (in campioni per canale) dei blocchi su cui viene campionato
/// il gain keyframeato in fase di mix: stessa granularità control-rate
/// (~60Hz) già usata dall'anteprima in tempo reale
/// (`apply_active_clip_gain` in `main.rs`), non sample-accurate.
const GAIN_BLOCK_FRAMES: usize = 800;

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
    fn advance_to(&mut self, target: FrameIdx) -> Result<Option<vv_media::FrameRgba>, String> {
        loop {
            match self.decoder.next_frame().map_err(|e| e.to_string())? {
                Some((idx, frame)) if idx >= target => return Ok(Some(frame)),
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// Cammina l'intera timeline (frame `0..total_frames`) e produce
/// `output_path`. Bloccante: il chiamante (`main.rs`) lo gira su un thread
/// dedicato. `project` è uno snapshot clonato al momento del click, non
/// condiviso con la UI — modificare il progetto durante l'export non lo
/// tocca.
pub fn export_timeline(
    project: &Project,
    timeline_id: TimelineId,
    output_path: &Path,
    progress: &Mutex<ExportProgress>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let timeline = project
        .timelines
        .get(timeline_id)
        .ok_or_else(|| "timeline non trovata".to_string())?;

    let total_frames = timeline.total_frames();
    progress.lock().unwrap().total_frames = total_frames;
    if total_frames <= 0 {
        progress.lock().unwrap().done = true;
        return Ok(());
    }

    let has_audio_track = timeline
        .tracks
        .get(AUDIO_TRACK)
        .is_some_and(|t| !t.clips.is_empty());

    let mut encoder = vv_media::Encoder::new(
        output_path,
        timeline.resolution.0,
        timeline.resolution.1,
        timeline.fps,
        has_audio_track.then_some((PROJECT_SAMPLE_RATE, PROJECT_CHANNELS as u16)),
    )
    .map_err(|e| e.to_string())?;

    // Compositor indipendente, di proprietà di questo thread: evita
    // qualunque contesa GPU col device della UI (che ha il suo,
    // `Compositor::new_headless()` in `main.rs`).
    let compositor = vv_render::Compositor::new_headless();

    let mut active: Option<ActiveClipDecoder> = None;
    for frame in 0..total_frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("annullato".to_string());
        }

        let rgba = render_video_frame(
            project,
            timeline,
            &compositor,
            &mut active,
            frame,
            timeline.resolution,
        )?;
        encoder
            .write_video_frame(&rgba)
            .map_err(|e| e.to_string())?;

        progress.lock().unwrap().current_frame = frame + 1;
    }

    if has_audio_track {
        let mixed = mix_audio_track(project, timeline, total_frames)?;
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
    active: &mut Option<ActiveClipDecoder>,
    frame: FrameIdx,
    resolution: (u32, u32),
) -> Result<Vec<u8>, String> {
    let Some(clip) = timeline.active_clip_at(VIDEO_TRACK, frame) else {
        *active = None;
        return Ok(black_frame(resolution));
    };

    // Mappatura clip→frame-sorgente condivisa con l'anteprima
    // (`vv_core::Clip::source_frame_at`, vedi doc lì per il perché —
    // REFACTOR_PIPELINE.md B1).
    let source_frame = clip.source_frame_at(frame);

    match &clip.source {
        ClipSource::Media(media_id) => {
            let path = project
                .media_pool
                .get(*media_id)
                .ok_or_else(|| "media non trovato nel pool".to_string())?
                .path
                .clone();

            if active.as_ref().is_none_or(|a| a.clip_id != clip.id) {
                *active = Some(ActiveClipDecoder::open_for(clip.id, &path, source_frame)?);
            }
            let source_frame_rgba = active
                .as_mut()
                .expect("appena assegnato sopra se assente")
                .advance_to(source_frame)?;

            let transform = clip.effects.transform.value_at(source_frame);
            Ok(match source_frame_rgba {
                Some(f) => compositor.render_frame(
                    &f.data,
                    f.width,
                    f.height,
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
            *active = None;
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

/// Decodifica/gaina/mixa la track audio (1) in un unico buffer PCM f32
/// interleaved a `PROJECT_SAMPLE_RATE`/`PROJECT_CHANNELS`, lungo
/// `total_frames` (nello spazio frame della timeline) — silenzio nei buchi
/// e dove non c'è traccia audio.
fn mix_audio_track(
    project: &Project,
    timeline: &Timeline,
    total_frames: FrameIdx,
) -> Result<Vec<f32>, String> {
    let fps = timeline.fps.as_f64().max(1e-9);
    let total_out_frames =
        (total_frames as f64 / fps * PROJECT_SAMPLE_RATE as f64).round() as usize;
    let mut mixed = vec![0.0_f32; total_out_frames * PROJECT_CHANNELS];

    let Some(track) = timeline.tracks.get(AUDIO_TRACK) else {
        return Ok(mixed);
    };

    for clip in &track.clips {
        let ClipSource::Media(media_id) = &clip.source else {
            continue; // SolidColor non ha audio.
        };
        let Some(item) = project.media_pool.get(*media_id) else {
            continue;
        };
        let Some(audio) = vv_media::decode_audio_track(&item.path).map_err(|e| e.to_string())?
        else {
            continue;
        };
        if audio.samples.is_empty() {
            continue;
        }

        let src_channels = audio.channels as usize;
        let src_rate = audio.sample_rate as f64;
        let clip_fps = item.meta.fps.as_f64().max(1e-9);

        // [source_in, source_out) del clip, tradotto da frame sorgente
        // (fps nativo del media) a campioni nel buffer decodificato.
        let start = ((clip.source_in as f64 / clip_fps * src_rate).round() as usize * src_channels)
            .min(audio.samples.len());
        let end = ((clip.source_out as f64 / clip_fps * src_rate).round() as usize * src_channels)
            .min(audio.samples.len());
        if start >= end {
            continue;
        }
        let mut slice = audio.samples[start..end].to_vec();

        apply_gain(
            &mut slice,
            src_channels,
            audio.sample_rate,
            clip_fps,
            clip.source_in,
            &clip.effects.gain_db,
        );

        let resampled = resample_and_remix(
            &slice,
            src_channels,
            audio.sample_rate,
            PROJECT_CHANNELS,
            PROJECT_SAMPLE_RATE,
        );

        let dst_start_frame =
            (clip.timeline_start as f64 / fps * PROJECT_SAMPLE_RATE as f64).round() as usize;
        let dst_start = dst_start_frame * PROJECT_CHANNELS;
        for (i, &s) in resampled.iter().enumerate() {
            if let Some(m) = mixed.get_mut(dst_start + i) {
                *m += s;
            }
        }
    }

    Ok(mixed)
}

/// Applica il gain (dB, keyframeato) a `samples` (interleaved,
/// `channels` canali, `src_rate` Hz) — a blocchi di `GAIN_BLOCK_FRAMES`
/// campioni per canale, campionando `gain_db` una volta per blocco al
/// frame sorgente corrispondente (non sample-accurate, stessa granularità
/// control-rate già usata dall'anteprima in tempo reale). `source_in` è
/// l'offset del clip nello spazio frame sorgente: `samples[0]` corrisponde
/// esattamente a quel frame.
fn apply_gain(
    samples: &mut [f32],
    channels: usize,
    src_rate: u32,
    clip_fps: f64,
    source_in: FrameIdx,
    gain_db: &Keyframed<f32>,
) {
    if channels == 0 {
        return;
    }
    if gain_db.is_constant() {
        let linear = db_to_linear(gain_db.value_at(0));
        if linear != 1.0 {
            for s in samples.iter_mut() {
                *s *= linear;
            }
        }
        return;
    }

    let block_len = GAIN_BLOCK_FRAMES * channels;
    let mut offset = 0;
    while offset < samples.len() {
        let end = (offset + block_len).min(samples.len());
        let secs = (offset / channels) as f64 / src_rate as f64;
        let source_frame = source_in + (secs * clip_fps).round() as FrameIdx;
        let linear = db_to_linear(gain_db.value_at(source_frame));
        for s in &mut samples[offset..end] {
            *s *= linear;
        }
        offset += block_len;
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Ricampiona (interpolazione lineare) e normalizza il numero di canali di
/// un buffer PCM f32 interleaved. Non un resampler professionale (niente
/// filtro anti-aliasing): per l'export va bene — la sorgente è già stata
/// decodificata a piena qualità, e la differenza percettibile è minima sul
/// contenuto tipico (parlato/musica) di un progetto di editing. Se
/// rate/canali coincidono già è un no-op (nessuna copia sprecata oltre
/// quella per canali diversi).
fn resample_and_remix(
    samples: &[f32],
    src_channels: usize,
    src_rate: u32,
    dst_channels: usize,
    dst_rate: u32,
) -> Vec<f32> {
    let remixed: Vec<f32> = match (src_channels, dst_channels) {
        (a, b) if a == b => samples.to_vec(),
        (1, 2) => samples.iter().flat_map(|&s| [s, s]).collect(),
        (2, 1) => samples
            .chunks_exact(2)
            .map(|c| (c[0] + c[1]) * 0.5)
            .collect(),
        (src, dst) if src > 0 => samples
            .chunks_exact(src)
            .flat_map(|frame| std::iter::repeat_n(frame[0], dst))
            .collect(),
        _ => Vec::new(),
    };

    if src_rate == dst_rate || dst_channels == 0 {
        return remixed;
    }

    let src_frames = remixed.len() / dst_channels;
    if src_frames == 0 {
        return Vec::new();
    }
    let dst_frames = (src_frames as f64 * dst_rate as f64 / src_rate as f64).round() as usize;
    let mut out = Vec::with_capacity(dst_frames * dst_channels);
    for i in 0..dst_frames {
        let src_pos = i as f64 * src_rate as f64 / dst_rate as f64;
        let i0 = (src_pos.floor() as usize).min(src_frames - 1);
        let i1 = (i0 + 1).min(src_frames - 1);
        let t = (src_pos - i0 as f64) as f32;
        for ch in 0..dst_channels {
            let a = remixed[i0 * dst_channels + ch];
            let b = remixed[i1 * dst_channels + ch];
            out.push(a + (b - a) * t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{Clip, EffectStack, Track, TrackKind};

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
            linked: None,
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

    #[test]
    fn render_video_frame_returns_black_in_a_gap() {
        let project = Project::default();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 10, 5, red())],
            muted: false,
        }]);
        let compositor = vv_render::Compositor::new_headless();
        let mut active = None;
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
        let mut active = None;
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
        let mixed = mix_audio_track(&project, &tl, 25).unwrap();
        assert_eq!(
            mixed.len(),
            (PROJECT_SAMPLE_RATE as usize) * PROJECT_CHANNELS
        );
        assert!(mixed.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn resample_and_remix_is_a_no_op_when_rate_and_channels_already_match() {
        let samples = vec![0.1, 0.2, 0.3, 0.4];
        let out = resample_and_remix(&samples, 2, 48000, 2, 48000);
        assert_eq!(out, samples);
    }

    #[test]
    fn resample_and_remix_duplicates_mono_to_stereo() {
        let samples = vec![0.5, -0.5];
        let out = resample_and_remix(&samples, 1, 48000, 2, 48000);
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn resample_and_remix_averages_stereo_to_mono() {
        let samples = vec![1.0, 0.0, 0.0, 1.0];
        let out = resample_and_remix(&samples, 2, 48000, 1, 48000);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn resample_and_remix_changes_frame_count_proportionally_to_rate() {
        let samples: Vec<f32> = (0..100).map(|i| i as f32).collect(); // mono, 100 frame
        let out = resample_and_remix(&samples, 1, 100, 1, 50);
        // Dimezzando il rate, ci si aspetta circa la metà dei frame.
        assert!((45..=55).contains(&out.len()), "len={}", out.len());
    }

    #[test]
    fn apply_gain_at_zero_db_is_a_no_op() {
        let mut samples = vec![0.5_f32, -0.3, 0.2, 0.1];
        let gain = Keyframed::constant(0.0_f32);
        apply_gain(&mut samples, 2, 48000, 25.0, 0, &gain);
        assert_eq!(samples, vec![0.5, -0.3, 0.2, 0.1]);
    }

    #[test]
    fn apply_gain_scales_samples_by_the_linear_equivalent_of_the_db_value() {
        let mut samples = vec![1.0_f32; 4];
        let gain = Keyframed::constant(-6.0206_f32); // ~metà ampiezza
        apply_gain(&mut samples, 1, 48000, 25.0, 0, &gain);
        for s in samples {
            assert!((s - 0.5).abs() < 0.001, "s={s}");
        }
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
        export_timeline(&app.project, timeline_id, &output_path, &progress, &cancel)
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

        let audio = vv_media::decode_audio_track(&output_path)
            .unwrap()
            .expect("audio atteso nell'export");
        let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }
}
