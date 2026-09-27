//! Saving and loading the project in RON.

use crate::model::Project;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("RON error: {0}")]
    Ron(#[from] ron::Error),
    #[error("RON error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
}

pub fn save_project(project: &Project, path: &Path) -> Result<(), PersistenceError> {
    let contents = ron::ser::to_string_pretty(project, ron::ser::PrettyConfig::default())?;
    std::fs::write(path, contents)?;
    Ok(())
}

pub fn load_project(path: &Path) -> Result<Project, PersistenceError> {
    let contents = std::fs::read_to_string(path)?;
    let mut project: Project = ron::from_str(&contents)?;
    // `Clip::rate` is derived from the fps: recomputing it here fixes
    // projects saved before the field existed (clips at an fps different
    // from the timeline's, out of sync).
    project.refresh_clip_rates();
    Ok(project)
}

#[cfg(test)]
#[path = "tests/persistence.rs"]
mod tests;
