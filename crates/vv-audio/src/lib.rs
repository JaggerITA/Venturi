//! Mix delle track audio (gain, resample, time-stretch) e output `cpal`
//! (vedi ARCHITECTURE.md § Pipeline audio). La posizione del mixer è il
//! clock del playback.

pub mod mixer;
pub mod stretch;

pub use mixer::{MixSnapshot, Mixer, MixerState, StretchedWindow};
pub use stretch::stretch_samples;
