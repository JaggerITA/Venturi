//! Audio track mixing (gain, resample, time-stretch) and `cpal` output
//! (see ARCHITECTURE.md § Audio pipeline). The mixer position is the
//! playback clock.

pub mod mixer;
pub mod stretch;

pub use mixer::{
    AnalysisSlot, AudioSource, ClipAudio, MixSnapshot, Mixer, MixerState, PeakAnalysis,
    PeakReading, StretchedWindow,
};
pub use stretch::stretch_samples;
