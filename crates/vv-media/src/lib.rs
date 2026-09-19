//! Probe, decode, cache dei frame, encode, proxy e waveform (vedi
//! ARCHITECTURE.md). Proxy e waveform sono cache su disco a chiave
//! `content_hash`.

pub mod audio;
pub mod cache;
pub mod decode;
pub mod encode;
pub mod probe;
pub mod proxy;
pub mod thumbnail;
pub mod waveform;

pub use audio::{AudioBuffer, decode_audio_track, decode_audio_streams_streaming};
pub use cache::{SharedFrameCache, WantedRange};
pub use decode::{ColorMatrix, Decoder, FrameYuv420, yuv420_frame_bytes};
pub use encode::{AudioCodec, AudioSettings, Encoder, VideoCodec, VideoSettings};
pub use thumbnail::{Thumbnail, generate_thumbnail};
pub use probe::{
    AUDIO_ONLY_FPS, AudioStreamInfo, IMAGE_FPS, audio_streams, content_fingerprint, probe,
    probe_image,
};
pub use waveform::{
    generate_waveforms, load_waveform, recommended_num_peaks, waveform_exists, waveform_path_for,
    Waveform,
};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),
    #[error("nessuno stream video/audio trovato in {0}")]
    NoStream(String),
    #[error("operazione annullata")]
    Cancelled,
    #[error("errore di I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Cartella di cache globale `$XDG_CACHE_HOME/vibevideo/<name>` (o
/// `~/.cache/...`, o la temp di sistema senza `HOME`).
pub(crate) fn cache_dir(name: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("vibevideo").join(name)
}

/// Fixture dei test (anche di vv-app): genera `output` con la CLI di ffmpeg.
#[doc(hidden)]
pub mod test_support {
    pub fn ffmpeg(args: &[&str], output: &std::path::Path) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(output)
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success(), "ffmpeg {args:?} fallito");
    }
}
