//! Decode, cache dei frame, proxy e waveform (vedi ARCHITECTURE.md).
//! Per ora implementato solo il probing dei media (milestone 1); il resto
//! dei moduli è uno scheletro da riempire nelle milestone successive.

pub mod cache;
pub mod decode;
pub mod probe;
pub mod proxy;
pub mod waveform;

pub use decode::{FrameRgba, decode_first_frame};
pub use probe::probe;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg_next::Error),
    #[error("nessuno stream video/audio trovato in {0}")]
    NoStream(String),
}
