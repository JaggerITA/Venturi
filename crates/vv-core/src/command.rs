//! Command pattern per undo/redo. Ogni comando cattura da sé lo stato
//! necessario a invertirsi nel momento in cui viene applicato.

use crate::model::{Clip, ClipId, FrameIdx, Interpolation, Project, TimelineId, Transform};

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

/// Sposta una clip a una nuova posizione, eventualmente su un'altra track
/// (drag nella timeline). Non fa collision-avoidance da sé: il chiamante
/// (la UI) deve clampare `new_start` prima di emettere il comando, per
/// mantenere l'invariante "clip mai sovrapposte sulla stessa track".
#[derive(Debug)]
pub struct MoveClip {
    pub timeline: TimelineId,
    pub clip_id: ClipId,
    pub from_track: usize,
    pub to_track: usize,
    pub new_start: FrameIdx,
    old_start: Option<FrameIdx>,
}

impl MoveClip {
    pub fn new(
        timeline: TimelineId,
        clip_id: ClipId,
        from_track: usize,
        to_track: usize,
        new_start: FrameIdx,
    ) -> Self {
        Self {
            timeline,
            clip_id,
            from_track,
            to_track,
            new_start,
            old_start: None,
        }
    }
}

impl Command for MoveClip {
    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        let from = &mut tl.tracks[self.from_track];
        let Some(pos) = from.clips.iter().position(|c| c.id == self.clip_id) else {
            return;
        };
        let mut clip = from.clips.remove(pos);
        self.old_start = Some(clip.timeline_start);
        clip.timeline_start = self.new_start;

        let to = &mut tl.tracks[self.to_track];
        let insert_at = to
            .clips
            .partition_point(|c| c.timeline_start < clip.timeline_start);
        to.clips.insert(insert_at, clip);
    }

    fn undo(&self, project: &mut Project) {
        let Some(old_start) = self.old_start else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        let to = &mut tl.tracks[self.to_track];
        let Some(pos) = to.clips.iter().position(|c| c.id == self.clip_id) else {
            return;
        };
        let mut clip = to.clips.remove(pos);
        clip.timeline_start = old_start;

        let from = &mut tl.tracks[self.from_track];
        let insert_at = from
            .clips
            .partition_point(|c| c.timeline_start < clip.timeline_start);
        from.clips.insert(insert_at, clip);
    }
}

/// Divide una clip in due al tempo di timeline `split_at`. La seconda metà
/// riceve un nuovo `ClipId`. Assume speed=1 nel mappare `split_at` allo
/// spazio del frame sorgente (coerente finché lo speed ramping non è
/// implementato, milestone 7).
#[derive(Debug)]
pub struct SplitClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub split_at: FrameIdx,
    original_source_out: Option<FrameIdx>,
    new_clip_id: Option<ClipId>,
}

impl SplitClip {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        split_at: FrameIdx,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            split_at,
            original_source_out: None,
            new_clip_id: None,
        }
    }
}

impl Command for SplitClip {
    fn apply(&mut self, project: &mut Project) {
        let new_id = project.alloc_clip_id();
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        if self.split_at <= clip.timeline_start || self.split_at >= clip.timeline_end() {
            return; // fuori dal corpo della clip: niente da dividere
        }

        let offset = self.split_at - clip.timeline_start;
        let split_source = clip.source_in + offset;

        self.original_source_out = Some(clip.source_out);
        let mut second_half = clip.clone();
        clip.source_out = split_source;

        second_half.id = new_id;
        second_half.source_in = split_source;
        second_half.timeline_start = self.split_at;
        self.new_clip_id = Some(new_id);

        let insert_at = track
            .clips
            .partition_point(|c| c.timeline_start < second_half.timeline_start);
        track.clips.insert(insert_at, second_half);
    }

    fn undo(&self, project: &mut Project) {
        let (Some(original_source_out), Some(new_clip_id)) =
            (self.original_source_out, self.new_clip_id)
        else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        track.clips.retain(|c| c.id != new_clip_id);
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.source_out = original_source_out;
        }
    }
}

/// Imposta il transform (crop/zoom/position) *statico* di una clip, cioè
/// `effects.transform.default` (milestone 5, prima parte: valori statici,
/// i keyframe arrivano dopo — vedi ARCHITECTURE.md § Milestone 5).
#[derive(Debug)]
pub struct SetClipTransform {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub new_value: Transform,
    old_value: Option<Transform>,
}

impl SetClipTransform {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        new_value: Transform,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            new_value,
            old_value: None,
        }
    }
}

impl Command for SetClipTransform {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        self.old_value = Some(clip.effects.transform.default);
        clip.effects.transform.default = self.new_value;
    }

    fn undo(&self, project: &mut Project) {
        let Some(old_value) = self.old_value else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.effects.transform.default = old_value;
        }
    }
}

/// Imposta il gain audio *statico* di una clip (dB), stesso schema di
/// `SetClipTransform`.
#[derive(Debug)]
pub struct SetClipGain {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub new_value: f32,
    old_value: Option<f32>,
}

impl SetClipGain {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId, new_value: f32) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            new_value,
            old_value: None,
        }
    }
}

impl Command for SetClipGain {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        self.old_value = Some(clip.effects.gain_db.default);
        clip.effects.gain_db.default = self.new_value;
    }

    fn undo(&self, project: &mut Project) {
        let Some(old_value) = self.old_value else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.effects.gain_db.default = old_value;
        }
    }
}

/// Il parametro animabile a cui si applica un `UpsertKeyframe`/
/// `RemoveKeyframe`. Due soli comandi invece di uno per parametro: quando
/// arriveranno altri parametri keyframeable (es. speed) basterà aggiungere
/// una variante qui, non un'altra coppia di comandi.
#[derive(Debug, Clone, Copy)]
pub enum KeyframeValue {
    Transform(Transform),
    Gain(f32),
}

/// Inserisce o sostituisce un keyframe di `effects.transform` o
/// `effects.gain_db` al frame indicato (milestone 5, seconda parte).
#[derive(Debug)]
pub struct UpsertKeyframe {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub frame: FrameIdx,
    pub value: KeyframeValue,
    pub interpolation: Interpolation,
    /// Valore/interpolazione che c'era prima a questo stesso frame, se
    /// c'era: `None` significa che il frame non aveva un keyframe, quindi
    /// l'undo deve rimuoverlo anziché ripristinarne uno vecchio.
    previous: Option<(KeyframeValue, Interpolation)>,
}

impl UpsertKeyframe {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        frame: FrameIdx,
        value: KeyframeValue,
        interpolation: Interpolation,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            frame,
            value,
            interpolation,
            previous: None,
        }
    }
}

impl Command for UpsertKeyframe {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        match self.value {
            KeyframeValue::Transform(v) => {
                self.previous = clip
                    .effects
                    .transform
                    .keyframe_at(self.frame)
                    .map(|(v, i)| (KeyframeValue::Transform(v), i));
                clip.effects
                    .transform
                    .upsert(self.frame, v, self.interpolation);
            }
            KeyframeValue::Gain(v) => {
                self.previous = clip
                    .effects
                    .gain_db
                    .keyframe_at(self.frame)
                    .map(|(v, i)| (KeyframeValue::Gain(v), i));
                clip.effects
                    .gain_db
                    .upsert(self.frame, v, self.interpolation);
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        match &self.previous {
            Some((KeyframeValue::Transform(v), i)) => {
                clip.effects.transform.upsert(self.frame, *v, *i);
            }
            Some((KeyframeValue::Gain(v), i)) => {
                clip.effects.gain_db.upsert(self.frame, *v, *i);
            }
            None => match self.value {
                KeyframeValue::Transform(_) => {
                    clip.effects.transform.remove_at(self.frame);
                }
                KeyframeValue::Gain(_) => {
                    clip.effects.gain_db.remove_at(self.frame);
                }
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum KeyframeTarget {
    Transform,
    Gain,
}

/// Rimuove il keyframe di `target` esattamente al frame indicato, se c'è.
#[derive(Debug)]
pub struct RemoveKeyframe {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub target: KeyframeTarget,
    pub frame: FrameIdx,
    removed: Option<(KeyframeValue, Interpolation)>,
}

impl RemoveKeyframe {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        target: KeyframeTarget,
        frame: FrameIdx,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            target,
            frame,
            removed: None,
        }
    }
}

impl Command for RemoveKeyframe {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        self.removed = match self.target {
            KeyframeTarget::Transform => clip
                .effects
                .transform
                .remove_at(self.frame)
                .map(|(v, i)| (KeyframeValue::Transform(v), i)),
            KeyframeTarget::Gain => clip
                .effects
                .gain_db
                .remove_at(self.frame)
                .map(|(v, i)| (KeyframeValue::Gain(v), i)),
        };
    }

    fn undo(&self, project: &mut Project) {
        let Some((value, interp)) = self.removed else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        match value {
            KeyframeValue::Transform(v) => clip.effects.transform.upsert(self.frame, v, interp),
            KeyframeValue::Gain(v) => clip.effects.gain_db.upsert(self.frame, v, interp),
        }
    }
}
