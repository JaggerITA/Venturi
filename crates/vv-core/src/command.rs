//! Command pattern per undo/redo. Ogni comando cattura da sé lo stato
//! necessario a invertirsi nel momento in cui viene applicato.

use crate::model::{Clip, ClipId, FrameIdx, Project, TimelineId};

pub trait Command: std::fmt::Debug {
    fn apply(&mut self, project: &mut Project);
    fn undo(&self, project: &mut Project);
}

#[derive(Default)]
pub struct History {
    undo_stack: Vec<Box<dyn Command>>,
    redo_stack: Vec<Box<dyn Command>>,
}

impl History {
    pub fn do_command(&mut self, project: &mut Project, mut cmd: Box<dyn Command>) {
        cmd.apply(project);
        self.undo_stack.push(cmd);
        self.redo_stack.clear();
    }

    pub fn undo(&mut self, project: &mut Project) {
        if let Some(cmd) = self.undo_stack.pop() {
            cmd.undo(project);
            self.redo_stack.push(cmd);
        }
    }

    pub fn redo(&mut self, project: &mut Project) {
        if let Some(mut cmd) = self.redo_stack.pop() {
            cmd.apply(project);
            self.undo_stack.push(cmd);
        }
    }
}

/// Inserisce una clip in una track a una posizione. Se sovrappone clip
/// esistenti, quelle sotto vengono spostate a destra (insert, non overwrite).
#[derive(Debug)]
pub struct InsertClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip: Clip,
}

impl Command for InsertClip {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let pos = track
            .clips
            .partition_point(|c| c.timeline_start < self.clip.timeline_start);
        track.clips.insert(pos, self.clip.clone());
    }

    fn undo(&self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        track.clips.retain(|c| c.id != self.clip.id);
    }
}

/// Normal delete ("lift"): rimuove la clip, lascia un vuoto al suo posto.
#[derive(Debug)]
pub struct LiftDelete {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    removed: Option<Clip>,
}

impl LiftDelete {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            removed: None,
        }
    }
}

impl Command for LiftDelete {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(pos) = track.clips.iter().position(|c| c.id == self.clip_id) {
            self.removed = Some(track.clips.remove(pos));
        }
    }

    fn undo(&self, project: &mut Project) {
        if let Some(clip) = &self.removed {
            let track = &mut project.timelines[self.timeline].tracks[self.track_index];
            let pos = track
                .clips
                .partition_point(|c| c.timeline_start < clip.timeline_start);
            track.clips.insert(pos, clip.clone());
        }
    }
}

/// Ripple delete globale: rimuove la clip e chiude il gap su *tutte* le
/// track della Timeline, mantenendo il sync audio/video.
#[derive(Debug)]
struct RippleState {
    clip: Clip,
    // Posizione originale (prima dello shift) di ogni clip toccata su ogni
    // track, per un undo esatto: non si può ricostruire lo shift da una
    // sola soglia perché, dopo aver sottratto `gap_len`, un valore shiftato
    // può risultare identico o minore di uno non toccato (vedi test).
    shifted: Vec<(usize, ClipId, FrameIdx)>, // (track_index, clip_id, original_start)
}

#[derive(Debug)]
pub struct RippleDeleteAllTracks {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    removed: Option<RippleState>,
}

impl RippleDeleteAllTracks {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            removed: None,
        }
    }
}

fn resort(track: &mut crate::model::Track) {
    track.clips.sort_by_key(|c| c.timeline_start);
}

impl Command for RippleDeleteAllTracks {
    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        let track = &mut tl.tracks[self.track_index];
        let Some(pos) = track.clips.iter().position(|c| c.id == self.clip_id) else {
            return;
        };
        let clip = track.clips.remove(pos);
        let gap_start = clip.timeline_start;
        let gap_len = clip.timeline_len();

        let mut shifted = Vec::new();
        for (track_index, track) in tl.tracks.iter_mut().enumerate() {
            for c in &mut track.clips {
                if c.timeline_start >= gap_start {
                    shifted.push((track_index, c.id, c.timeline_start));
                    c.timeline_start -= gap_len;
                }
            }
            resort(track);
        }
        self.removed = Some(RippleState { clip, shifted });
    }

    fn undo(&self, project: &mut Project) {
        let Some(state) = &self.removed else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];

        for (track_index, clip_id, original_start) in &state.shifted {
            let track = &mut tl.tracks[*track_index];
            if let Some(c) = track.clips.iter_mut().find(|c| c.id == *clip_id) {
                c.timeline_start = *original_start;
            }
        }
        let track = &mut tl.tracks[self.track_index];
        track.clips.push(state.clip.clone());
        for track in &mut tl.tracks {
            resort(track);
        }
    }
}
