//! Decode, cache dei frame, proxy e waveform (vedi ARCHITECTURE.md).
//! Milestone 2: decode sequenziale/seek (`decode`), frame cache LRU
//! (`cache`), decode-ahead in background (`playback`) e decodifica della
//! traccia audio (`audio`). `proxy`/`waveform` restano scheletri per le
//! milestone successive.

pub mod audio;
pub mod cache;
pub mod decode;
pub mod encode;
pub mod playback;
pub mod probe;
pub mod proxy;
pub mod waveform;

pub use audio::{AudioBuffer, decode_audio_track};
pub use cache::{FrameCache, SharedFrameCache, WantedRange};
pub use decode::{ColorMatrix, Decoder, FrameYuv420, decode_first_frame};
pub use encode::Encoder;
pub use playback::{DecodeAhead, frame_cache_capacity};
pub use probe::{content_fingerprint, probe};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),
    #[error("nessuno stream video/audio trovato in {0}")]
    NoStream(String),
}
