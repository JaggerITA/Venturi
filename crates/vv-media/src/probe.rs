//! Lettura dei metadata di un media.

use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::Once;
use vv_core::{MediaMeta, Rational};

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
    Ok(MediaMeta {
        duration_frames: (duration_secs * fps.as_f64()).round() as i64,
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
