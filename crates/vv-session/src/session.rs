use std::path::{Path, PathBuf};

use vv_core::{
    AddEntities, Command, CommandLabel, History, MediaFileInfo, PersistenceError, Project,
    Timeline, TimelineId,
};

use std::collections::HashMap;

use crate::jobs::{Jobs, Waker};

/// An open document: the project, its history and where it is saved.
/// `project` and `history` are public so callers can borrow them apart;
/// every change goes through the history, which is what tells a saved
/// project from a modified one.
#[derive(Default)]
pub struct Session {
    pub project: Project,
    pub history: History,
    path: Option<PathBuf>,
    saved_generation: u64,
    synced_timeline_generation: u64,
    /// Bumped whenever the project is replaced.
    epoch: u64,
    /// `timeline_revision` per timeline, with the state it was computed at.
    revisions: std::sync::Mutex<HashMap<TimelineId, (ChangeMark, String)>>,
    pub(crate) waker: Waker,
    pub(crate) jobs: Jobs,
}

/// Where the project stands: equal marks mean nothing changed in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeMark {
    epoch: u64,
    generation: u64,
}

impl Session {
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    pub fn apply(&mut self, cmd: Box<dyn Command>) {
        self.history.do_command(&mut self.project, cmd);
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.history.generation() != self.saved_generation
    }

    pub fn mark_saved(&mut self) {
        self.saved_generation = self.history.generation();
    }

    pub fn change_mark(&self) -> ChangeMark {
        ChangeMark {
            epoch: self.epoch,
            generation: self.history.generation(),
        }
    }

    /// Bumped each time the project is replaced (opened, new).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A fingerprint of the timeline's content: it changes with any edit of
    /// that timeline, not with edits elsewhere. Recomputed only after the
    /// project changed.
    pub fn timeline_revision(&self, id: TimelineId) -> String {
        let mark = self.change_mark();
        let mut revisions = self.revisions.lock().unwrap();
        if let Some((at, revision)) = revisions.get(&id)
            && *at == mark
        {
            return revision.clone();
        }
        use std::hash::{Hash, Hasher};
        let bytes = self
            .project
            .timelines
            .get(id)
            .and_then(|timeline| serde_json::to_vec(timeline).ok())
            .unwrap_or_default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        let revision = format!("{:016x}", hasher.finish());
        revisions.insert(id, (mark, revision.clone()));
        revision
    }

    /// Saved `project`, a copy of this session's taken at `mark`: the
    /// session counts as saved only if nothing changed since.
    pub fn finish_save(&mut self, path: PathBuf, mark: ChangeMark) {
        self.path = Some(path);
        if mark == self.change_mark() {
            self.mark_saved();
        } else if mark.epoch == self.epoch {
            self.saved_generation = mark.generation;
        }
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
    pub fn replace_project(&mut self, mut project: Project, path: Option<PathBuf>) {
        complete_legacy_media(&mut project);
        self.install_project(project, path);
    }

    /// Like `replace_project`, for a project `complete_legacy_media` already
    /// went through (it reads the media files: slow, better done first).
    pub fn install_project(&mut self, project: Project, path: Option<PathBuf>) {
        self.project = project;
        self.history = History::default();
        // The new history counts from 0 again.
        self.synced_timeline_generation = self.history.generation();
        self.path = path;
        self.epoch += 1;
        self.mark_saved();
    }

    /// Adds a timeline and its media pool entry, through which it can be
    /// used as a clip in other timelines.
    pub fn create_timeline(&mut self, timeline: Timeline) -> TimelineId {
        let mut add = AddEntities::new(CommandLabel::NewTimeline);
        let (id, _) = add.timeline(&mut self.project, timeline, None);
        self.apply(Box::new(add));
        id
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

/// Fills in the media fields that projects saved by older versions lack,
/// reading the files while they are still reachable.
pub fn complete_legacy_media(project: &mut Project) {
    for item in project.media_pool.values_mut() {
        if item.compound.is_some() {
            continue;
        }
        // Projects saved before `MediaMeta::audio_streams`.
        if item.meta.has_audio && item.meta.audio_streams == 0 {
            item.meta.audio_streams =
                vv_media::audio_streams(&item.path).map_or(1, |s| s.len() as u16);
        }
        // Projects saved before `MediaMeta::file`.
        if item.meta.file == MediaFileInfo::default()
            && let Ok(file) = vv_media::probe_file_info(&item.path)
        {
            item.meta.file = file;
        }
    }
}

#[cfg(test)]
#[path = "tests/session.rs"]
mod tests;
