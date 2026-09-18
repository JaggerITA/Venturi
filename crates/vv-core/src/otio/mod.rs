//! OpenTimelineIO (`.otio`, JSON): export di una timeline e import di un
//! file in un progetto nuovo.

mod export;
mod import;

pub use export::{export_otio, timeline_to_otio};
pub use import::{OtioImport, import_otio, project_from_otio};

#[derive(Debug, thiserror::Error)]
pub enum OtioError {
    #[error("errore di I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("errore JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Format(String),
}
