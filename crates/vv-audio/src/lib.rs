//! Decode audio (in vv-media) -> resample -> gain -> time-stretch -> mix ->
//! output `cpal` (vedi ARCHITECTURE.md § Pipeline audio). L'audio è il
//! clock master durante il playback.

pub mod mixer;
pub mod output;
pub mod stretch;

pub use mixer::{MixSnapshot, Mixer};
pub use output::AudioPlayer;
pub use stretch::stretch_samples;
