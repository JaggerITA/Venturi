//! Decode audio (in vv-media) -> resample -> gain -> time-stretch -> mix ->
//! output `cpal` (vedi ARCHITECTURE.md § Pipeline audio). L'audio è il
//! clock master durante il playback.
//!
//! Scheletro per la milestone 2 (mixing/output) e 7 (time-stretch). Ancora
//! da implementare.

pub mod mixer;
pub mod output;
pub mod stretch;
