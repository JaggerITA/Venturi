//! OpenTimelineIO (`.otio`, JSON): export di una timeline e import di un
//! file in un progetto nuovo.

mod export;
mod import;

pub use export::{export_otio, timeline_to_otio};
pub use import::{OtioImport, OtioWarning, import_otio, project_from_otio};

#[derive(Debug, thiserror::Error)]
pub enum OtioError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Format(String),
}
