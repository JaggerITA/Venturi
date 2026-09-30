//! Audio track mixing (gain, resample, time-stretch) and `cpal` output
//! (see ARCHITECTURE.md § Audio pipeline). The mixer position is the
//! playback clock.

pub mod dynamics;
pub mod eq;
mod filter;
pub mod mixer;
pub mod recorder;
pub mod spectrum;
pub mod stretch;

pub use mixer::{
    AnalysisSlot, AudioSource, BandMeter, ClipAudio, MixSnapshot, Mixer, MixerState, PeakAnalysis,
    PeakReading, StretchedWindow,
};
pub use stretch::stretch_samples;
