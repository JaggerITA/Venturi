//! Apertura di un media e lettura dei metadata (milestone 1).

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

pub fn probe(path: &Path) -> Result<MediaMeta, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;

    let video = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;

    let rate = video.rate();
    let fps = Rational::new(rate.numerator(), rate.denominator());

    let video_decoder = ffmpeg::codec::context::Context::from_parameters(video.parameters())?
        .decoder()
        .video()?;
    let width = video_decoder.width();
    let height = video_decoder.height();

    let duration_secs = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let duration_frames = (duration_secs * fps.as_f64()).round() as i64;

    let audio_stream = input.streams().best(ffmpeg::media::Type::Audio);
    let (has_audio, sample_rate, channels) = match audio_stream {
        Some(audio) => {
            let audio_decoder =
                ffmpeg::codec::context::Context::from_parameters(audio.parameters())?
                    .decoder()
                    .audio()?;
            (true, audio_decoder.rate(), audio_decoder.channels())
        }
        None => (false, 0, 0),
    };

    Ok(MediaMeta {
        duration_frames,
        fps,
        width,
        height,
        has_audio,
        sample_rate,
        channels,
    })
}

/// Info su uno stream audio del contenitore, per l'import multi-traccia
/// (vedi `audio_streams`).
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Enumera *tutti* gli stream audio del contenitore, nell'ordine in cui
/// compaiono (non l'euristica "best" di ffmpeg, che ne sceglie uno solo):
/// questo è l'ordine che `Clip::audio_stream_index` e i parametri
/// `stream_index` di `decode_audio_track`/`generate_waveform` si
/// aspettano — l'indice N-esimo in questo vettore è lo stream audio
/// N-esimo del file, a prescindere da quale sia "il migliore".
///
/// Un file con più tracce audio (es. un mix stereo *e* un 5.1 separato,
/// come capita con sorgenti broadcast) va importato con una clip audio
/// per ogni stream — vedi `VibeVideoApp::insert_media_clip` — invece di
/// tenerne una sola (quella "best") e perdere silenziosamente le altre.
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

/// Fingerprint economico di un file (path canonico + dimensione + data
/// di modifica, FNV-1a), usato come `MediaItem::content_hash` — chiave
/// dei proxy (`proxy.rs`) e di qualunque altra cache derivata dal
/// contenuto. Non un vero hash dei byte: leggere l'intero file
/// rallenterebbe ogni import su sorgenti da GB, e qui basta un
/// fingerprint "stesso file della volta scorsa", non una garanzia
/// crittografica — un file sovrascritto con la stessa dimensione e la
/// stessa mtime per coincidenza (raro: capita solo con strumenti di
/// copia che preservano i metadata alla lettera) userebbe una cache
/// stantia, un compromesso accettato esplicitamente per restare
/// istantaneo anche su file grandi. Stabile tra un riavvio dell'app e
/// l'altro (a differenza di un hash randomizzato per-processo come
/// `DefaultHasher`), non implementato su hardware/versioni rustc
/// diverse.
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
    use std::process::Command;

    /// Genera un vero file x264 (con audio AAC) via ffmpeg CLI e verifica
    /// che il probe legga metadata coerenti. Questo è il caso d'uso
    /// primario dichiarato dall'utente (compressed x264), non un mock.
    #[test]
    fn probe_reads_x264_metadata() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-y",
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
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success(), "generazione del file di test fallita");

        let meta = probe(&path).expect("probe fallito");
        assert_eq!(meta.width, 640);
        assert_eq!(meta.height, 360);
        assert_eq!(meta.fps, Rational::new(25, 1));
        assert!(meta.has_audio);
        assert_eq!(meta.sample_rate, 48000);
        // ~2s a 25fps: tollera qualche frame di arrotondamento sul container.
        assert!((meta.duration_frames - 50).abs() <= 2);
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
