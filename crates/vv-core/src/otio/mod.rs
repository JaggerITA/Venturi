//! OpenTimelineIO (`.otio`, JSON): exporting a timeline and importing a
//! file into a new project.

mod export;
mod generator;
mod import;
mod resolve;

pub use export::{export_otio, timeline_to_otio};

/// Size in timeline pixels of the box a title draws around its text. Only
/// whoever can shape the text knows it, so the export asks for it.
pub type MeasureTitle<'a> = &'a dyn Fn(&crate::model::TitleParams) -> (f32, f32);
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
