//! Decode audio (in vv-media) -> resample -> gain -> time-stretch -> mix ->
//! output `cpal` (vedi ARCHITECTURE.md § Pipeline audio). L'audio è il
//! clock master durante il playback.
//!
//! Milestone 2: `output::AudioPlayer` riproduce un buffer pre-decodificato
//! con play/pause/seek e una posizione leggibile come clock A/V. Mixing
//! multi-traccia e time-stretch restano scheletri per le milestone 3 e 7.

pub mod mixer;
pub mod output;
pub mod stretch;

pub use output::AudioPlayer;
pub use stretch::stretch_samples;
