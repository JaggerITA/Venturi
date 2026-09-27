//! Commands on the project's entities: media, timelines and folders. New
//! entities get their ids when the command is built, so a redo puts them
//! back under the ids later steps refer to.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::{
    ClipSource, Command, CommandLabel, FolderId, MediaFolder, MediaId, MediaItem, Project,
    Timeline, TimelineId,
};

/// Folders, timelines and media added to the project.
#[derive(Debug)]
pub struct AddEntities {
    label: CommandLabel,
    folders: Vec<(FolderId, MediaFolder)>,
    timelines: Vec<(TimelineId, Timeline)>,
    media: Vec<(MediaId, MediaItem)>,
}

impl AddEntities {
    pub fn new(label: CommandLabel) -> Self {
        Self {
            label,
            folders: Vec::new(),
            timelines: Vec::new(),
            media: Vec::new(),
        }
    }

    pub fn folder(&mut self, project: &mut Project, folder: MediaFolder) -> FolderId {
        let id = project.folders.alloc();
        self.folders.push((id, folder));
        id
    }

    pub fn media(&mut self, project: &mut Project, item: MediaItem) -> MediaId {
        let id = project.media_pool.alloc();
        self.media.push((id, item));
        id
    }

    /// A timeline and the pool item that uses it as a clip.
    pub fn timeline(
        &mut self,
        project: &mut Project,
        timeline: Timeline,
        folder: Option<FolderId>,
    ) -> (TimelineId, MediaId) {
        let timeline_id = project.timelines.alloc();
        let item = project.timeline_item(timeline_id, &timeline, folder);
        self.timelines.push((timeline_id, timeline));
        (timeline_id, self.media(project, item))
    }

    /// A timeline whose pool item the caller adds with `media`.
    pub fn bare_timeline(&mut self, project: &mut Project, timeline: Timeline) -> TimelineId {
        let id = project.timelines.alloc();
        self.timelines.push((id, timeline));
        id
    }

    pub fn media_ids(&self) -> impl Iterator<Item = MediaId> + '_ {
        self.media.iter().map(|(id, _)| *id)
    }
}

impl Command for AddEntities {
    fn label(&self) -> CommandLabel {
        self.label
    }

    fn apply(&mut self, project: &mut Project) {
        for (id, folder) in &self.folders {
            project.folders.insert_at(*id, folder.clone());
        }
        for (id, timeline) in &self.timelines {
            project.timelines.insert_at(*id, timeline.clone());
        }
        for (id, item) in &self.media {
            project.media_pool.insert_at(*id, item.clone());
        }
    }

    fn undo(&self, project: &mut Project) {
        for (id, _) in &self.media {
            project.media_pool.remove(*id);
        }
        for (id, _) in &self.timelines {
            project.timelines.remove(*id);
        }
        for (id, _) in &self.folders {
            project.folders.remove(*id);
        }
    }
}

/// Adds the media and timelines of `other` to `add`, its pool items in
/// `folder` (the folders of `other` are not carried over). A media of
/// `other` found in `reuse` is replaced by the one of `project` it maps to.
/// A timeline without a pool item gets one at the root. Returns the new
/// timelines, in `other`'s order.
pub fn absorb(
    project: &mut Project,
    add: &mut AddEntities,
    other: Project,
    reuse: &HashMap<MediaId, MediaId>,
    folder: Option<FolderId>,
) -> Vec<TimelineId> {
    let timeline_ids: HashMap<TimelineId, TimelineId> = other
        .timelines
        .keys()
        .map(|id| (id, project.timelines.alloc()))
        .collect();
    let mut media_ids = reuse.clone();
    let mut items = Vec::new();
    for (id, mut item) in other.media_pool {
        if media_ids.contains_key(&id) {
            continue;
        }
        item.folder = folder;
        item.compound = item.compound.and_then(|t| timeline_ids.get(&t).copied());
        let new_id = project.media_pool.alloc();
        media_ids.insert(id, new_id);
        items.push((new_id, item));
    }
    let mut timelines = Vec::new();
    for (id, mut timeline) in other.timelines {
        project.reallocate_clip_ids(&mut timeline);
        for clip in timeline.tracks.iter_mut().flat_map(|t| &mut t.clips) {
            if let ClipSource::Media(media) = &mut clip.source
                && let Some(&new_media) = media_ids.get(media)
            {
                *media = new_media;
            }
        }
        let new_id = timeline_ids[&id];
        match items
            .iter_mut()
            .find(|(_, item)| item.compound == Some(new_id))
        {
            Some((_, item)) => {
                item.meta = timeline.compound_meta();
                item.content_hash = project.alloc_compound_generation();
            }
            None => {
                let item = project.timeline_item(new_id, &timeline, None);
                items.push((project.media_pool.alloc(), item));
            }
        }
        add.timelines.push((new_id, timeline));
        timelines.push(new_id);
    }
    add.media.extend(items);
    timelines
}

/// A copy of the timeline of `source` (a compound pool item) as a new
/// pool item called `name`, in the same folder.
pub fn duplicate_timeline(
    project: &mut Project,
    source: MediaId,
    name: String,
) -> Option<(MediaId, AddEntities)> {
    let item = project.media_pool.get(source)?;
    let folder = item.folder;
    let mut timeline = project.timelines.get(item.compound?)?.clone();
    timeline.name = name;
    project.reallocate_clip_ids(&mut timeline);
    let mut add = AddEntities::new(CommandLabel::DuplicateTimeline);
    let (_, media_id) = add.timeline(project, timeline, folder);
    Some((media_id, add))
}

/// Renames a compound pool item together with its timeline.
#[derive(Debug)]
pub struct RenameTimeline {
    media: MediaId,
    name: String,
    old: RefCell<Option<(PathBuf, String)>>,
}

impl RenameTimeline {
    pub fn new(media: MediaId, name: String) -> Self {
        Self {
            media,
            name,
            old: RefCell::new(None),
        }
    }
}

impl Command for RenameTimeline {
    fn label(&self) -> CommandLabel {
        CommandLabel::RenameTimeline
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(item) = project.media_pool.get_mut(self.media) else {
            return;
        };
        let Some(timeline) = item.compound.and_then(|id| project.timelines.get_mut(id)) else {
            return;
        };
        let old_path = std::mem::replace(&mut item.path, self.name.clone().into());
        let old_name = std::mem::replace(&mut timeline.name, self.name.clone());
        *self.old.borrow_mut() = Some((old_path, old_name));
    }

    fn undo(&self, project: &mut Project) {
        let Some((path, name)) = self.old.borrow_mut().take() else {
            return;
        };
        let item = &mut project.media_pool[self.media];
        item.path = path;
        if let Some(timeline) = item.compound.and_then(|id| project.timelines.get_mut(id)) {
            timeline.name = name;
        }
    }
}

#[derive(Debug)]
pub struct RenameFolder {
    folder: FolderId,
    name: String,
    old: RefCell<Option<String>>,
}

impl RenameFolder {
    pub fn new(folder: FolderId, name: String) -> Self {
        Self {
            folder,
            name,
            old: RefCell::new(None),
        }
    }
}

impl Command for RenameFolder {
    fn label(&self) -> CommandLabel {
        CommandLabel::RenameFolder
    }

    fn apply(&mut self, project: &mut Project) {
        if let Some(folder) = project.folders.get_mut(self.folder) {
            *self.old.borrow_mut() = Some(std::mem::replace(&mut folder.name, self.name.clone()));
        }
    }

    fn undo(&self, project: &mut Project) {
        if let Some(name) = self.old.borrow_mut().take() {
            project.folders[self.folder].name = name;
        }
    }
}

/// Puts media in `folder` (`None`: the pool root).
#[derive(Debug)]
pub struct SetMediaFolder {
    media: Vec<MediaId>,
    folder: Option<FolderId>,
    old: RefCell<Vec<(MediaId, Option<FolderId>)>>,
}

impl SetMediaFolder {
    pub fn new(media: Vec<MediaId>, folder: Option<FolderId>) -> Self {
        Self {
            media,
            folder,
            old: RefCell::new(Vec::new()),
        }
    }
}

impl Command for SetMediaFolder {
    fn label(&self) -> CommandLabel {
        CommandLabel::MoveToFolder
    }

    fn apply(&mut self, project: &mut Project) {
        let mut old = self.old.borrow_mut();
        old.clear();
        for &id in &self.media {
            if let Some(item) = project.media_pool.get_mut(id) {
                old.push((id, std::mem::replace(&mut item.folder, self.folder)));
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        for (id, folder) in self.old.borrow_mut().drain(..) {
            project.media_pool[id].folder = folder;
        }
    }
}

/// Moves `folder` into `parent`: check `Project::can_move_folder` first.
#[derive(Debug)]
pub struct MoveFolder {
    folder: FolderId,
    parent: Option<FolderId>,
    old: RefCell<Option<Option<FolderId>>>,
}

impl MoveFolder {
    pub fn new(folder: FolderId, parent: Option<FolderId>) -> Self {
        Self {
            folder,
            parent,
            old: RefCell::new(None),
        }
    }
}

impl Command for MoveFolder {
    fn label(&self) -> CommandLabel {
        CommandLabel::MoveToFolder
    }

    fn apply(&mut self, project: &mut Project) {
        if let Some(folder) = project.folders.get_mut(self.folder) {
            *self.old.borrow_mut() = Some(std::mem::replace(&mut folder.parent, self.parent));
        }
    }

    fn undo(&self, project: &mut Project) {
        if let Some(parent) = self.old.borrow_mut().take() {
            project.folders[self.folder].parent = parent;
        }
    }
}

/// Removes `folder`, moving its media and subfolders to its parent.
#[derive(Debug)]
pub struct DeleteFolder {
    folder: FolderId,
    removed: RefCell<Option<RemovedFolder>>,
}

#[derive(Debug)]
struct RemovedFolder {
    folder: MediaFolder,
    media: Vec<MediaId>,
    children: Vec<FolderId>,
}

impl DeleteFolder {
    pub fn new(folder: FolderId) -> Self {
        Self {
            folder,
            removed: RefCell::new(None),
        }
    }
}

impl Command for DeleteFolder {
    fn label(&self) -> CommandLabel {
        CommandLabel::DeleteFolder
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(removed) = project.folders.remove(self.folder) else {
            return;
        };
        let mut media = Vec::new();
        for (id, item) in project.media_pool.iter_mut() {
            if item.folder == Some(self.folder) {
                item.folder = removed.parent;
                media.push(id);
            }
        }
        let mut children = Vec::new();
        for (id, child) in project.folders.iter_mut() {
            if child.parent == Some(self.folder) {
                child.parent = removed.parent;
                children.push(id);
            }
        }
        *self.removed.borrow_mut() = Some(RemovedFolder {
            folder: removed,
            media,
            children,
        });
    }

    fn undo(&self, project: &mut Project) {
        let Some(RemovedFolder {
            folder,
            media,
            children,
        }) = self.removed.borrow_mut().take()
        else {
            return;
        };
        project.folders.insert_at(self.folder, folder);
        for id in media {
            project.media_pool[id].folder = Some(self.folder);
        }
        for id in children {
            project.folders[id].parent = Some(self.folder);
        }
    }
}

#[cfg(test)]
#[path = "tests/pool.rs"]
mod tests;
