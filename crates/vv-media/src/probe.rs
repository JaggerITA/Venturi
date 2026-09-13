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
}
