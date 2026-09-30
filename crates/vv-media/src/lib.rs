//! Probe, decode, frame cache, encode, proxy and waveform (see
//! ARCHITECTURE.md). Proxy and waveform are on-disk caches keyed by
//! `content_hash`.

pub mod audio;
pub mod audio_file;
pub mod cache;
pub mod decode;
pub mod encode;
pub mod hw;
pub mod probe;
pub mod proxy;
pub mod thumbnail;
pub mod waveform;

pub use audio::{AudioBuffer, decode_audio_streams_streaming, decode_audio_track};
pub use cache::{SharedFrameCache, WantedRange};
pub use decode::{Chroma, ColorMatrix, Decoder, FrameYuv420, yuv420_frame_bytes};
pub use encode::{AudioCodec, AudioSettings, Encoder, VideoCodec, VideoSettings};
pub use hw::{HwDevice, HwPriority};
pub use probe::{
    AUDIO_ONLY_FPS, AudioStreamInfo, IMAGE_EXTENSIONS, IMAGE_FPS, audio_streams,
    content_fingerprint, is_image_path, probe, probe_file_info, probe_image, probe_media,
    probe_tags,
};
pub use thumbnail::{Thumbnail, generate_thumbnail};
pub use waveform::{
    Waveform, generate_waveforms, load_waveform, recommended_num_peaks, waveform_exists,
    waveform_path_for,
};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),
    #[error("no video/audio stream found in {0}")]
    NoStream(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Global cache directory `$XDG_CACHE_HOME/venturi/<name>` (or
/// `~/.cache/...`, or the system temp dir without `HOME`).
pub(crate) fn cache_dir(name: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("venturi").join(name)
}

/// Test fixture (vv-app's too): generates `output` with the ffmpeg CLI.
#[doc(hidden)]
pub mod test_support {
    pub fn ffmpeg(args: &[&str], output: &std::path::Path) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(output)
            .status()
            .expect("ffmpeg CLI not found");
        assert!(status.success(), "ffmpeg {args:?} failed");
    }
}
