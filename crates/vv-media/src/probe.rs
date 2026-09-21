//! Lettura dei metadata di un media.

use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::Once;
use vv_core::{FrameIdx, MediaMeta, Rational};

static INIT: Once = Once::new();

pub(crate) fn ensure_init() {
    INIT.call_once(|| {
        ffmpeg::init().expect("impossibile inizializzare ffmpeg");
    });
}

/// Fps nominale di un media solo audio: non ha frame propri, ma
/// `source_in`/`source_out` e i keyframe del gain si contano comunque in
/// frame sorgente.
pub const AUDIO_ONLY_FPS: Rational = Rational::new(30, 1);

/// Fps nominale di un'immagine: serve solo un'unità coerente per
/// `source_in`/`source_out`.
pub const IMAGE_FPS: Rational = Rational::new(25, 1);

pub fn probe(path: &Path) -> Result<MediaMeta, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;

    let video = match input.streams().best(ffmpeg::media::Type::Video) {
        Some(video) => {
            let rate = video.rate();
            let fps = Rational::new(rate.numerator(), rate.denominator());
            let decoder = ffmpeg::codec::context::Context::from_parameters(video.parameters())?
                .decoder()
                .video()?;
            Some((fps, decoder.width(), decoder.height()))
        }
        None => None,
    };

    let audio = input.streams().best(ffmpeg::media::Type::Audio);
    let (has_audio, sample_rate, channels) = match audio {
        Some(audio) => {
            let audio_decoder =
                ffmpeg::codec::context::Context::from_parameters(audio.parameters())?
                    .decoder()
                    .audio()?;
            (true, audio_decoder.rate(), audio_decoder.channels())
        }
        None => (false, 0, 0),
    };
    if video.is_none() && !has_audio {
        return Err(crate::MediaError::NoStream(path.display().to_string()));
    }

    let audio_streams = input
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
        .count() as u16;
    let (fps, width, height) = video.unwrap_or((AUDIO_ONLY_FPS, 0, 0));
    let duration_secs = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let nominal_duration_frames = (duration_secs * fps.as_f64()).round() as i64;
    // Il container spesso dichiara più frame di quanti il decoder ne
    // produca davvero (drop frame, indice CFR impreciso...): chi chiede
    // il frame `duration_frames - 1` come "ultimo frame" (es. il freeze
    // di `extrapolated_frame_for`) riceverebbe per sempre `None` se ci
    // fidassimo ciecamente del calcolo aritmetico.
    let duration_frames = if video.is_some() {
        verify_last_decodable_frame(path, fps, nominal_duration_frames)
    } else {
        nominal_duration_frames
    };
    Ok(MediaMeta {
        duration_frames,
        fps,
        width,
        height,
        has_video: video.is_some(),
        has_audio,
        sample_rate,
        channels,
        audio_streams,
    })
}

/// Corregge `nominal` (durata*fps) al vero conteggio di frame che lo
/// stream produce. Due passate, entrambe sulla sola coda del file: un
/// demux (nessuna decodifica) che trova il pts dell'ultimo pacchetto
/// video e dell'ultimo keyframe, poi la decodifica del solo ultimo GOP
/// per confermare che quel frame esca davvero dal decoder. Non alza mai
/// `nominal`: se qualcosa fallisce si tiene il valore aritmetico e
/// l'eventuale eccedenza si ignora — questo fix corregge solo la
/// sovrastima, non ne introduce una nuova.
fn verify_last_decodable_frame(path: &Path, fps: Rational, nominal: i64) -> i64 {
    let idx_of = |secs: f64| (secs * fps.as_f64()).round() as FrameIdx;
    let Some((last_packet_secs, last_key_secs)) = scan_tail(path, fps, nominal) else {
        return nominal;
    };
    match decode_from(path, last_key_secs) {
        Some(secs) => (idx_of(secs) + 1).min(nominal),
        // Il GOP finale non si decodifica: ci si ferma al demux, che è
        // comunque più vicino al vero del calcolo aritmetico.
        None => (idx_of(last_packet_secs) + 1).min(nominal),
    }
}

/// `(pts dell'ultimo pacchetto video, pts dell'ultimo keyframe)` in
/// secondi, demuxando dalla coda dello stream. Se il seek atterra oltre
/// la vera fine (è il caso che stiamo correggendo) si rilegge dall'inizio:
/// senza decodifica costa comunque pochi ms anche su file lunghi.
fn scan_tail(path: &Path, fps: Rational, nominal: i64) -> Option<(f64, f64)> {
    const SAFETY_MARGIN_SECS: f64 = 15.0;
    let nominal_secs = nominal as f64 / fps.as_f64();
    for seek_secs in [(nominal_secs - SAFETY_MARGIN_SECS).max(0.0), 0.0] {
        let mut input = ffmpeg::format::input(&path).ok()?;
        let stream = input.streams().best(ffmpeg::media::Type::Video)?;
        let stream_index = stream.index();
        let time_base = seconds_per_tick(stream.time_base());
        let ts = (seek_secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        if input.seek(ts, ..ts).is_err() {
            continue;
        }
        let mut last = None;
        let mut last_key = 0.0f64;
        for (stream, packet) in input.packets() {
            if stream.index() != stream_index {
                continue;
            }
            let Some(secs) = packet.pts().map(|pts| pts as f64 * time_base) else {
                continue;
            };
            last = Some(last.map_or(secs, |max: f64| max.max(secs)));
            if packet.is_key() {
                last_key = last_key.max(secs);
            }
        }
        if let Some(last) = last {
            return Some((last, last_key));
        }
        if seek_secs == 0.0 {
            break;
        }
    }
    None
}

/// Pts dell'ultimo frame che il decoder produce partendo dal keyframe a
/// `from_secs`, in secondi. Nessuno scaler: serve solo il timestamp, non
/// i pixel.
fn decode_from(path: &Path, from_secs: f64) -> Option<f64> {
    let mut input = ffmpeg::format::input(&path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let stream_index = stream.index();
    let time_base = seconds_per_tick(stream.time_base());
    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder();
    context.set_threading(ffmpeg::threading::Config {
        kind: ffmpeg::threading::Type::Frame,
        count: 0,
        ..Default::default()
    });
    let mut decoder = context.video().ok()?;
    let ts = (from_secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
    input.seek(ts, ..ts).ok()?;

    let mut frame = ffmpeg::frame::Video::empty();
    let mut last: Option<f64> = None;
    let mut drain = |decoder: &mut ffmpeg::decoder::Video, last: &mut Option<f64>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            let Some(secs) = frame.pts().map(|pts| pts as f64 * time_base) else {
                continue;
            };
            *last = Some(last.map_or(secs, |max: f64| max.max(secs)));
        }
    };
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_ok() {
            drain(&mut decoder, &mut last);
        }
    }
    if decoder.send_eof().is_ok() {
        drain(&mut decoder, &mut last);
    }
    last
}

fn seconds_per_tick(time_base: ffmpeg::Rational) -> f64 {
    time_base.numerator() as f64 / time_base.denominator() as f64
}

/// Come `probe` per un'immagine: `duration_frames` è il sentinel
/// `vv_core::IMAGE_DURATION_FRAMES`.
pub fn probe_image(path: &Path) -> Result<MediaMeta, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;
    let video = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
    let decoder = ffmpeg::codec::context::Context::from_parameters(video.parameters())?
        .decoder()
        .video()?;
    Ok(MediaMeta {
        duration_frames: vv_core::IMAGE_DURATION_FRAMES,
        fps: IMAGE_FPS,
        width: decoder.width(),
        height: decoder.height(),
        has_video: true,
        has_audio: false,
        sample_rate: 0,
        channels: 0,
        audio_streams: 0,
    })
}

/// Info su uno stream audio del contenitore, per l'import multi-traccia
/// (vedi `audio_streams`).
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Tutti gli stream audio nell'ordine del contenitore (non il "best" di
/// ffmpeg): è l'indice di `Clip::audio_stream_index`. Un file multi-audio si
/// importa con una clip per stream.
pub fn audio_streams(path: &Path) -> Result<Vec<AudioStreamInfo>, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;
    let mut out = Vec::new();
    for stream in input.streams() {
        if stream.parameters().medium() != ffmpeg::media::Type::Audio {
            continue;
        }
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        out.push(AudioStreamInfo {
            sample_rate: decoder.rate(),
            channels: decoder.channels(),
        });
    }
    Ok(out)
}

/// Fingerprint (path canonico + dimensione + mtime, FNV-1a) usato come
/// `content_hash` per le cache derivate. Non legge i byte: un file
/// sovrascritto con stessa dimensione e mtime userebbe una cache stantia,
/// compromesso accettato per import istantanei. Stabile tra riavvii, a
/// differenza di `DefaultHasher`.
pub fn content_fingerprint(path: &Path) -> std::io::Result<u64> {
    let metadata = std::fs::metadata(path)?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
        }
    };
    feed(canonical.to_string_lossy().as_bytes());
    feed(&metadata.len().to_le_bytes());
    feed(&mtime_secs.to_le_bytes());
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Riproduce lo scenario del bug: un container che dichiara più frame
    /// di quanti il decoder ne produca davvero (qui simulato passando a
    /// `verify_last_decodable_frame` un `nominal` gonfiato oltre il vero
    /// conteggio, invece di un container reale rotto). Deve correggerlo
    /// al vero ultimo frame decodificabile, non fidarsi del valore dato.
    #[test]
    fn verify_last_decodable_frame_corrects_an_inflated_nominal() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dieci_frame.mp4");
        crate::test_support::ffmpeg(
            &["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=1", "-c:v", "libx264", "-pix_fmt", "yuv420p"],
            &path,
        );

        let fps = Rational::new(10, 1);
        let real = verify_last_decodable_frame(&path, fps, 10);
        let corrected = verify_last_decodable_frame(&path, fps, 10_000);

        assert_eq!(corrected, real, "un nominal gonfiato deve convergere al vero ultimo frame decodificabile");
        assert!(corrected <= 15, "10 frame veri a 10fps: il conteggio corretto non deve restare vicino al nominal gonfiato ({corrected})");
    }

    /// Caso reale della sovrastima: l'audio dura più del video, quindi la
    /// durata del container (il massimo fra gli stream) moltiplicata per
    /// gli fps promette molti più frame di quanti il video ne abbia.
    #[test]
    fn probe_ignores_frames_promised_by_an_audio_track_longer_than_the_video() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audio_piu_lungo.mp4");
        crate::test_support::ffmpeg(
            &[
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=1",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=4",
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
            ],
            &path,
        );

        let meta = probe(&path).expect("probe fallito");
        assert!(
            (meta.duration_frames - 25).abs() <= 2,
            "1 s di video a 25 fps, non i frame promessi dai 4 s di audio: {}",
            meta.duration_frames
        );
    }

    /// Genera un vero file x264 (con audio AAC) via ffmpeg CLI e verifica
    /// che il probe legga metadata coerenti. Questo è il caso d'uso
    /// primario dichiarato dall'utente (compressed x264), non un mock.
    #[test]
    fn probe_reads_x264_metadata() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");

        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=25:duration=2",
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
            &path,
        );

        let meta = probe(&path).expect("probe fallito");
        assert_eq!(meta.width, 640);
        assert_eq!(meta.height, 360);
        assert_eq!(meta.fps, Rational::new(25, 1));
        assert!(meta.has_audio);
        assert_eq!(meta.sample_rate, 48000);
        assert_eq!(meta.audio_streams, 1);
        // ~2s a 25fps: tollera qualche frame di arrotondamento sul container.
        assert!((meta.duration_frames - 50).abs() <= 2);
    }

    #[test]
    fn probe_accepts_an_audio_only_file() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tono.wav");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=2",
            ],
            &path,
        );

        let meta = probe(&path).expect("probe fallito");
        assert!(!meta.has_video);
        assert!(meta.has_audio);
        assert_eq!((meta.width, meta.height), (0, 0));
        assert_eq!(meta.fps, AUDIO_ONLY_FPS);
        assert_eq!(meta.sample_rate, 44_100);
        assert_eq!(meta.duration_frames, 60, "2 s a fps nominale");
    }

    #[test]
    fn probe_image_reads_dimensions_and_reports_the_image_sentinel() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("still.png");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );

        let meta = probe_image(&path).expect("probe_image fallito");
        assert_eq!((meta.width, meta.height), (640, 360));
        assert_eq!(meta.fps, IMAGE_FPS);
        assert!(meta.has_video);
        assert!(!meta.has_audio);
        assert_eq!(meta.duration_frames, vv_core::IMAGE_DURATION_FRAMES);
        assert!(meta.is_image());
    }

    #[test]
    fn probe_rejects_a_file_without_audio_or_video() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sottotitoli.srt");
        std::fs::write(&path, "1\n00:00:00,000 --> 00:00:01,000\nciao\n").unwrap();
        assert!(probe(&path).is_err());
    }

    #[test]
    fn content_fingerprint_is_stable_for_the_same_unchanged_file() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stable.bin");
        std::fs::write(&path, "contenuto di prova").unwrap();

        let a = content_fingerprint(&path).unwrap();
        let b = content_fingerprint(&path).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn content_fingerprint_differs_for_different_sized_files() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.bin");
        let path_b = dir.join("b.bin");
        std::fs::write(&path_a, "contenuto corto").unwrap();
        std::fs::write(&path_b, "contenuto molto più lungo di quello corto").unwrap();

        assert_ne!(
            content_fingerprint(&path_a).unwrap(),
            content_fingerprint(&path_b).unwrap()
        );
    }

    #[test]
    fn content_fingerprint_changes_when_the_file_is_rewritten_with_different_content() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rewritten.bin");

        std::fs::write(&path, "prima versione").unwrap();
        let before = content_fingerprint(&path).unwrap();

        // Dimensione diversa garantisce che il fingerprint cambi anche
        // se il filesystem ha una risoluzione della mtime troppo bassa
        // per registrare la scrittura come "più tardi" della precedente
        // entro la durata del test.
        std::fs::write(&path, "seconda versione, più lunga della prima").unwrap();
        let after = content_fingerprint(&path).unwrap();

        assert_ne!(before, after);
    }
}
