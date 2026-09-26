//! OpenTimelineIO (`.otio`, JSON): exporting a timeline and importing a
//! file into a new project.

mod export;
mod generator;
mod import;
mod resolve;

pub use export::{export_otio, timeline_to_otio};

/// Size of the text block of a title and the padding its background adds
/// around it, in timeline pixels. Only whoever can shape the text knows
/// them, so the export asks for them.
#[derive(Debug, Clone, Copy)]
pub struct TitleMetrics {
    pub block: (f32, f32),
    pub padding: f32,
}

pub type MeasureTitle<'a> = &'a dyn Fn(&crate::model::TitleParams) -> TitleMetrics;
pub use import::{OtioImport, OtioWarning, import_otio, media_url_count, project_from_otio};

#[derive(Debug, thiserror::Error)]
pub enum OtioError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Format(String),
}
