use std::path::{Path, PathBuf};

use vv_core::{History, MediaFileInfo, PersistenceError, Project, TimelineId};

use crate::jobs::{Jobs, Waker};

/// An open document: the project, its history and where it is saved.
/// `project` and `history` are public so callers can borrow them apart;
/// changes that bypass the history must call `mark_unsaved`.
#[derive(Default)]
pub struct Session {
    pub project: Project,
    pub history: History,
    path: Option<PathBuf>,
    saved_generation: u64,
    /// The media pool, folders or timeline list changed outside the history.
    changed_outside_history: bool,
    synced_timeline_generation: u64,
    pub(crate) waker: Waker,
    pub(crate) jobs: Jobs,
}

impl Session {
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.changed_outside_history || self.history.generation() != self.saved_generation
    }

    pub fn mark_saved(&mut self) {
        self.saved_generation = self.history.generation();
        self.changed_outside_history = false;
    }

    pub fn mark_unsaved(&mut self) {
        self.changed_outside_history = true;
    }

    pub fn save_to(&mut self, path: &Path) -> Result<(), PersistenceError> {
        vv_core::save_project(&self.project, path)?;
        self.path = Some(path.to_path_buf());
        self.mark_saved();
        Ok(())
    }

    pub fn open(&mut self, path: &Path) -> Result<(), PersistenceError> {
        let project = vv_core::load_project(path)?;
        self.replace_project(project, Some(path.to_path_buf()));
        Ok(())
    }

    pub fn new_project(&mut self) {
        self.replace_project(Project::default(), None);
    }

    /// Starts over on `project` (opened from `path`, or new): empty history,
    /// nothing unsaved. Fills in the media fields older projects lack.
    pub fn replace_project(&mut self, project: Project, path: Option<PathBuf>) {
        self.project = project;
        self.history = History::default();
        // The new history counts from 0 again.
        self.synced_timeline_generation = self.history.generation();
        self.path = path;
        self.mark_saved();
        for item in self.project.media_pool.values_mut() {
            if item.compound.is_some() {
                continue;
            }
            // Projects saved before `MediaMeta::audio_streams`.
            if item.meta.has_audio && item.meta.audio_streams == 0 {
                item.meta.audio_streams =
                    vv_media::audio_streams(&item.path).map_or(1, |s| s.len() as u16);
            }
            // Projects saved before `MediaMeta::file`: record it while the
            // file is still reachable.
            if item.meta.file == MediaFileInfo::default()
                && let Ok(file) = vv_media::probe_file_info(&item.path)
            {
                item.meta.file = file;
            }
        }
    }

    /// Keeps the media pool entry of `timeline_id` (its duration, video and
    /// audio presence) in step with its content after edits. Only once per
    /// history change: resyncing gives the entry a new `content_hash`.
    pub fn sync_timeline_media(&mut self, timeline_id: TimelineId) {
        let generation = self.history.generation();
        if generation == self.synced_timeline_generation {
            return;
        }
        self.synced_timeline_generation = generation;
        let Some(media_id) = self
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound == Some(timeline_id))
            .map(|(id, _)| id)
        else {
            return;
        };
        self.project.sync_compound_meta(media_id);
    }
}

#[cfg(test)]
#[path = "tests/session.rs"]
mod tests;
