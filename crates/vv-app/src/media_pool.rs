//! Multiple selection in the media pool, with the same rules as the timeline:
//! plain click, ctrl+click to add/remove, shift+click for a range, rubber-band
//! selection by dragging from the background. The selection logic lives here
//! as a pure function (testable without `egui::Ui`); the panel drawing stays
//! in `main.rs`.

use std::collections::{BTreeSet, HashSet};
use vv_core::{FolderId, MediaId};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

#[derive(Default)]
pub struct MediaPoolState {
    pub selected: BTreeSet<MediaId>,
    /// Range origin for the next shift+click (which does *not* move it,
    /// as in `timeline_ui::TimelineState::selection_anchor`).
    anchor: Option<MediaId>,
    /// Selection rectangle in progress, in screen coordinates.
    pub marquee: Option<(egui::Pos2, egui::Pos2)>,
    /// The media pool got the last click: Del/Backspace deletes the selected
    /// media instead of the clips on the timeline.
    pub focused: bool,
    pub sort: Sort,
    pub renaming: Option<Rename>,
    /// A click on the name of the only selected item, with its time: it
    /// becomes a rename unless a double click follows (the "slow double
    /// click").
    pub rename_pending: Option<(MediaId, f64)>,
    pub expanded: HashSet<FolderId>,
    /// Residual touchpad inertia (px/s), see `timeline_ui::apply_kinetic_scroll`.
    pub scroll_vel: f32,
    /// The pool's ScrollArea as of the last frame: its offset is driven
    /// before `show`, when the current frame's geometry is not known yet.
    pub scroll_area: Option<PoolScrollArea>,
}

#[derive(Clone, Copy)]
pub struct PoolScrollArea {
    pub id: egui::Id,
    pub viewport: egui::Rect,
    pub max_offset: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenameTarget {
    Media(MediaId),
    Folder(FolderId),
}

pub struct Rename {
    pub target: RenameTarget,
    pub text: String,
    /// Set until the text field has been shown once: it then takes focus
    /// with the whole name selected.
    pub just_started: bool,
}

impl MediaPoolState {
    /// Drops what refers to media or folders no longer in `project`.
    pub fn forget_removed(&mut self, project: &vv_core::Project) {
        let media_exists = |id: MediaId| project.media_pool.contains_key(id);
        self.selected.retain(|&id| media_exists(id));
        self.anchor = self.anchor.filter(|&id| media_exists(id));
        self.rename_pending = self.rename_pending.filter(|&(id, _)| media_exists(id));
        if self.renaming.as_ref().is_some_and(|r| match r.target {
            RenameTarget::Media(id) => !media_exists(id),
            RenameTarget::Folder(id) => !project.folders.contains_key(id),
        }) {
            self.renaming = None;
        }
        self.expanded.retain(|&id| project.folders.contains_key(id));
    }

    /// `order` is the order the items are drawn in the panel: that is what
    /// defines the range of a shift+click.
    pub fn click(&mut self, clicked: MediaId, modifiers: egui::Modifiers, order: &[MediaId]) {
        let (selected, anchor) =
            apply_click(&self.selected, self.anchor, clicked, modifiers, order);
        self.selected = selected;
        self.anchor = anchor;
    }

    /// Replaces the selection with `hits`, in drawing order: the first
    /// becomes the anchor for a possible later shift+click.
    pub fn select_only(&mut self, hits: impl IntoIterator<Item = MediaId>) {
        let hits: Vec<MediaId> = hits.into_iter().collect();
        self.anchor = hits.first().copied();
        self.selected = hits.into_iter().collect();
    }

    /// Click on a column header: sorts by that criterion, or flips the
    /// direction if it was already the active one.
    pub fn toggle_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort = Sort {
                key,
                ascending: true,
            };
        }
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }
}

/// Sorts the panel items according to `sort`. On equal duration the order
/// stays the one by name, so the list does not jump on every redraw.
pub fn sort_items<T>(
    items: &mut [T],
    sort: Sort,
    name: impl Fn(&T) -> &str,
    duration_secs: impl Fn(&T) -> f64,
) {
    items.sort_by(|a, b| {
        let by_name = natural_cmp(name(a), name(b));
        let ord = match sort.key {
            SortKey::Name => by_name,
            SortKey::Duration => duration_secs(a)
                .partial_cmp(&duration_secs(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(by_name),
        };
        if sort.ascending { ord } else { ord.reverse() }
    });
}

/// Drag payload of a folder row.
pub struct FolderDrag(pub FolderId);

pub enum PoolRow<T> {
    Folder { id: FolderId, depth: usize },
    Item { item: T, depth: usize },
}

/// The pool as drawn: at each level the folders (by name) and then the
/// items, in the order given; the content of collapsed folders is left out.
/// A missing parent folder counts as the root.
pub fn tree_rows<T>(
    folders: &[(FolderId, &str, Option<FolderId>)],
    items: Vec<(T, Option<FolderId>)>,
    expanded: &HashSet<FolderId>,
) -> Vec<PoolRow<T>> {
    let exists =
        |folder: Option<FolderId>| folder.filter(|f| folders.iter().any(|(id, ..)| id == f));
    let mut sorted: Vec<_> = folders.to_vec();
    sorted.sort_by(|a, b| natural_cmp(a.1, b.1));
    let mut items: Vec<(Option<T>, Option<FolderId>)> = items
        .into_iter()
        .map(|(item, folder)| (Some(item), exists(folder)))
        .collect();
    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    push_level(
        None,
        0,
        &sorted,
        &mut items,
        expanded,
        &mut visited,
        &mut rows,
        &exists,
    );
    rows
}

#[allow(clippy::too_many_arguments)]
fn push_level<T>(
    parent: Option<FolderId>,
    depth: usize,
    folders: &[(FolderId, &str, Option<FolderId>)],
    items: &mut [(Option<T>, Option<FolderId>)],
    expanded: &HashSet<FolderId>,
    visited: &mut HashSet<FolderId>,
    rows: &mut Vec<PoolRow<T>>,
    exists: &dyn Fn(Option<FolderId>) -> Option<FolderId>,
) {
    for &(id, _, folder_parent) in folders {
        // `visited`: a corrupted file with a parent cycle must not recurse forever.
        if exists(folder_parent) != parent || !visited.insert(id) {
            continue;
        }
        rows.push(PoolRow::Folder { id, depth });
        if expanded.contains(&id) {
            push_level(
                Some(id),
                depth + 1,
                folders,
                items,
                expanded,
                visited,
                rows,
                exists,
            );
        }
    }
    for (item, folder) in items.iter_mut() {
        if *folder == parent
            && let Some(item) = item.take()
        {
            rows.push(PoolRow::Item { item, depth });
        }
    }
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

fn apply_click(
    current: &BTreeSet<MediaId>,
    anchor: Option<MediaId>,
    clicked: MediaId,
    modifiers: egui::Modifiers,
    order: &[MediaId],
) -> (BTreeSet<MediaId>, Option<MediaId>) {
    if modifiers.shift {
        let range = anchor
            .and_then(|a| {
                let from = order.iter().position(|id| *id == a)?;
                let to = order.iter().position(|id| *id == clicked)?;
                Some(order[from.min(to)..=from.max(to)].iter().copied().collect())
            })
            .unwrap_or_else(|| BTreeSet::from([clicked]));
        (range, anchor.or(Some(clicked)))
    } else if modifiers.command {
        let mut set = current.clone();
        if !set.remove(&clicked) {
            set.insert(clicked);
        }
        (set, Some(clicked))
    } else {
        (BTreeSet::from([clicked]), Some(clicked))
    }
}

#[cfg(test)]
#[path = "tests/media_pool.rs"]
mod tests;
