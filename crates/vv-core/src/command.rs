//! Command pattern per undo/redo. Ogni comando cattura da sé lo stato
//! necessario a invertirsi nel momento in cui viene applicato.

use crate::model::{
    Clip, ClipFilter, ClipId, ClipSource, CrossTransition, EffectStack, FrameIdx, Interpolation, Keyframed,
    LinkGroupId, MediaId, MediaItem, Project, Rgba, TimelineId, TitleParams, Track, TrackKind, Transform,
    TransformParam, Transition,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub trait Command: std::fmt::Debug {
    fn apply(&mut self, project: &mut Project);
    fn undo(&self, project: &mut Project);
    fn label(&self) -> CommandLabel;
}

/// Nome di un passo di history; il testo tradotto lo sceglie l'app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLabel {
    AddTrack,
    RemoveTrack,
    MuteTrack,
    SoloTrack,
    LockTrack,
    ToggleClipsDisabled,
    InsertClips,
    PasteClips,
    DuplicateClips,
    DeleteClips,
    RippleDelete,
    MoveClips,
    TrimClips,
    UnlinkClips,
    LinkClips,
    SplitClips,
    Fade,
    Transform,
    Flip,
    Gain,
    ResetGain,
    Title,
    ResetTransform,
    ClipColor,
    SetKeyframe,
    RemoveKeyframe,
    RemoveMedia,
    RelinkMedia,
    Filters,
    Transition,
}

/// Più comandi in un solo passo di history.
#[derive(Debug)]
pub struct CompositeCommand {
    commands: Vec<Box<dyn Command>>,
    label: CommandLabel,
}

impl CompositeCommand {
    pub fn new(label: CommandLabel, commands: Vec<Box<dyn Command>>) -> Self {
        Self { commands, label }
    }
}

impl Command for CompositeCommand {
    fn label(&self) -> CommandLabel {
        self.label
    }

    fn apply(&mut self, project: &mut Project) {
        for cmd in &mut self.commands {
            cmd.apply(project);
        }
    }

    fn undo(&self, project: &mut Project) {
        for cmd in self.commands.iter().rev() {
            cmd.undo(project);
        }
    }
}

/// Punto di inizio di un gruppo di undo, vedi `History::begin_group`.
#[derive(Debug, Clone, Copy)]
pub struct GroupMark(usize);

#[derive(Default)]
pub struct History {
    undo_stack: Vec<Box<dyn Command>>,
    redo_stack: Vec<Box<dyn Command>>,
    generation: u64,
}

impl History {
    pub fn do_command(&mut self, project: &mut Project, mut cmd: Box<dyn Command>) {
        cmd.apply(project);
        self.undo_stack.push(cmd);
        self.redo_stack.clear();
        self.generation += 1;
    }

    /// Da qui a `end_group` i comandi diventano un unico passo di undo: per
    /// un'azione che si scompone in più comandi (es. un drop di più media).
    pub fn begin_group(&mut self) -> GroupMark {
        GroupMark(self.undo_stack.len())
    }

    /// Il gruppo prende il nome del suo primo comando.
    pub fn end_group(&mut self, mark: GroupMark) {
        self.close_group(mark, None);
    }

    /// Per i gruppi il cui primo comando è accessorio (es. la track creata
    /// al volo da un drop) e non darebbe il nome giusto.
    pub fn end_group_as(&mut self, mark: GroupMark, label: CommandLabel) {
        self.close_group(mark, Some(label));
    }

    fn close_group(&mut self, mark: GroupMark, label: Option<CommandLabel>) {
        let commands = self.undo_stack.split_off(mark.0.min(self.undo_stack.len()));
        if commands.len() > 1 {
            let label = label.unwrap_or_else(|| commands[0].label());
            self.undo_stack
                .push(Box::new(CompositeCommand::new(label, commands)));
        } else {
            self.undo_stack.extend(commands);
        }
    }

    pub fn undo(&mut self, project: &mut Project) {
        if let Some(cmd) = self.undo_stack.pop() {
            cmd.undo(project);
            self.redo_stack.push(cmd);
            self.generation += 1;
        }
    }

    pub fn redo(&mut self, project: &mut Project) {
        if let Some(mut cmd) = self.redo_stack.pop() {
            cmd.apply(project);
            self.undo_stack.push(cmd);
            self.generation += 1;
        }
    }

    /// Tutti i passi, dal più vecchio: i primi `position()` sono applicati,
    /// gli altri si possono rifare.
    pub fn labels(&self) -> impl Iterator<Item = CommandLabel> + '_ {
        self.undo_stack
            .iter()
            .chain(self.redo_stack.iter().rev())
            .map(|cmd| cmd.label())
    }

    pub fn position(&self) -> usize {
        self.undo_stack.len()
    }

    /// Annulla o rifà fino a lasciare applicati i primi `position` passi.
    pub fn go_to(&mut self, project: &mut Project, position: usize) {
        while self.undo_stack.len() > position {
            self.undo(project);
        }
        while self.undo_stack.len() < position && !self.redo_stack.is_empty() {
            self.redo(project);
        }
    }

    /// Cambia a ogni modifica effettiva: basta confrontarla per sapere se il
    /// progetto è cambiato, senza diffarlo.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Aggiunge una track vuota in coda: per il compositing conta solo
/// l'ordine relativo tra track video.
#[derive(Debug)]
pub struct AddTrack {
    pub timeline: TimelineId,
    pub kind: TrackKind,
    /// Indice assegnato da `apply` (l'ultimo di `tracks` in quel momento),
    /// noto solo a posteriori.
    index: Option<usize>,
}

impl AddTrack {
    pub fn new(timeline: TimelineId, kind: TrackKind) -> Self {
        Self {
            timeline,
            kind,
            index: None,
        }
    }

    /// L'indice della track appena creata, noto solo dopo `apply`.
    pub fn track_index(&self) -> Option<usize> {
        self.index
    }
}

impl Command for AddTrack {
    fn label(&self) -> CommandLabel {
        CommandLabel::AddTrack
    }

    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        tl.tracks.push(Track::new(self.kind));
        self.index = Some(tl.tracks.len() - 1);
    }

    fn undo(&self, project: &mut Project) {
        let Some(index) = self.index else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        if index < tl.tracks.len() {
            tl.tracks.remove(index);
        }
    }
}

/// Rimuove una track con le sue clip. Tenerne almeno una per tipo è una
/// regola della UI, non del modello.
#[derive(Debug)]
pub struct RemoveTrack {
    pub timeline: TimelineId,
    pub track_index: usize,
    removed: Option<Track>,
}

impl RemoveTrack {
    pub fn new(timeline: TimelineId, track_index: usize) -> Self {
        Self {
            timeline,
            track_index,
            removed: None,
        }
    }
}

impl Command for RemoveTrack {
    fn label(&self) -> CommandLabel {
        CommandLabel::RemoveTrack
    }

    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        if self.track_index >= tl.tracks.len() {
            return;
        }
        self.removed = Some(tl.tracks.remove(self.track_index));
    }

    fn undo(&self, project: &mut Project) {
        let Some(track) = self.removed.clone() else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        let index = self.track_index.min(tl.tracks.len());
        tl.tracks.insert(index, track);
    }
}

/// Stato di una track modificabile dalla sua intestazione.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackFlag {
    Muted,
    Solo,
    Locked,
}

impl TrackFlag {
    fn field(self, track: &mut Track) -> &mut bool {
        match self {
            TrackFlag::Muted => &mut track.muted,
            TrackFlag::Solo => &mut track.solo,
            TrackFlag::Locked => &mut track.locked,
        }
    }
}

#[derive(Debug)]
pub struct SetTrackFlag {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub flag: TrackFlag,
    pub value: bool,
    old: Option<bool>,
}

impl SetTrackFlag {
    pub fn new(timeline: TimelineId, track_index: usize, flag: TrackFlag, value: bool) -> Self {
        Self {
            timeline,
            track_index,
            flag,
            value,
            old: None,
        }
    }
}

impl Command for SetTrackFlag {
    fn label(&self) -> CommandLabel {
        match self.flag {
            TrackFlag::Muted => CommandLabel::MuteTrack,
            TrackFlag::Solo => CommandLabel::SoloTrack,
            TrackFlag::Locked => CommandLabel::LockTrack,
        }
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(track) = project.timelines[self.timeline].tracks.get_mut(self.track_index) else {
            return;
        };
        let field = self.flag.field(track);
        self.old = Some(*field);
        *field = self.value;
    }

    fn undo(&self, project: &mut Project) {
        let (Some(old), Some(track)) =
            (self.old, project.timelines[self.timeline].tracks.get_mut(self.track_index))
        else {
            return;
        };
        *self.flag.field(track) = old;
    }
}

/// Attiva o disattiva (tasto D) un insieme di clip.
#[derive(Debug)]
pub struct SetClipsDisabled {
    pub timeline: TimelineId,
    pub clips: Vec<(usize, ClipId)>,
    pub disabled: bool,
    old: Vec<(usize, ClipId, bool)>,
}

impl SetClipsDisabled {
    pub fn new(timeline: TimelineId, clips: Vec<(usize, ClipId)>, disabled: bool) -> Self {
        Self {
            timeline,
            clips,
            disabled,
            old: Vec::new(),
        }
    }
}

impl Command for SetClipsDisabled {
    fn label(&self) -> CommandLabel {
        CommandLabel::ToggleClipsDisabled
    }

    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        self.old.clear();
        for &(track_index, clip_id) in &self.clips {
            if let Some(clip) = tl.clip_mut(track_index, clip_id) {
                self.old.push((track_index, clip_id, clip.disabled));
                clip.disabled = self.disabled;
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        for &(track_index, clip_id, old) in &self.old {
            if let Some(clip) = tl.clip_mut(track_index, clip_id) {
                clip.disabled = old;
            }
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
    fn label(&self) -> CommandLabel {
        CommandLabel::InsertClips
    }

    fn apply(&mut self, project: &mut Project) {
        project.timelines[self.timeline].tracks[self.track_index].insert_sorted(self.clip.clone());
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
    fn label(&self) -> CommandLabel {
        CommandLabel::DeleteClips
    }

    fn apply(&mut self, project: &mut Project) {
        self.removed =
            project.timelines[self.timeline].tracks[self.track_index].remove_clip(self.clip_id);
    }

    fn undo(&self, project: &mut Project) {
        if let Some(clip) = &self.removed {
            project.timelines[self.timeline].tracks[self.track_index].insert_sorted(clip.clone());
        }
    }
}

fn resort(track: &mut Track) {
    track.clips.sort_by_key(|c| c.timeline_start);
}

/// Chiude uno spazio vuoto (nessuna clip da rimuovere) su tutte le track,
/// shiftando indietro di `gap_len` ogni clip che inizia a `gap_start` o
/// dopo — mantiene il sync A/V globale.
#[derive(Debug)]
pub struct RippleDeleteGap {
    pub timeline: TimelineId,
    pub gap_start: FrameIdx,
    pub gap_len: FrameIdx,
    shifted: Vec<(usize, ClipId, FrameIdx)>,
}

impl RippleDeleteGap {
    pub fn new(timeline: TimelineId, gap_start: FrameIdx, gap_len: FrameIdx) -> Self {
        Self {
            timeline,
            gap_start,
            gap_len,
            shifted: Vec::new(),
        }
    }
}

impl Command for RippleDeleteGap {
    fn label(&self) -> CommandLabel {
        CommandLabel::RippleDelete
    }

    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        let mut shifted = Vec::new();
        for (track_index, track) in tl.tracks.iter_mut().enumerate() {
            if track.locked {
                continue;
            }
            for c in &mut track.clips {
                if c.timeline_start >= self.gap_start {
                    shifted.push((track_index, c.id, c.timeline_start));
                    c.timeline_start -= self.gap_len;
                }
            }
            resort(track);
        }
        self.shifted = shifted;
    }

    fn undo(&self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        for (track_index, clip_id, original_start) in &self.shifted {
            if let Some(c) = tl.clip_mut(*track_index, *clip_id) {
                c.timeline_start = *original_start;
            }
        }
        for track in &mut tl.tracks {
            resort(track);
        }
    }
}

/// Sposta più clip in un solo passo di history (es. un gruppo trascinato).
#[derive(Debug)]
pub struct MoveClips {
    pub timeline: TimelineId,
    /// (clip_id, from_track, to_track, new_start)
    pub moves: Vec<(ClipId, usize, usize, FrameIdx)>,
    old_starts: Vec<Option<FrameIdx>>,
}

impl MoveClips {
    pub fn new(timeline: TimelineId, moves: Vec<(ClipId, usize, usize, FrameIdx)>) -> Self {
        Self {
            timeline,
            moves,
            old_starts: Vec::new(),
        }
    }
}

impl Command for MoveClips {
    fn label(&self) -> CommandLabel {
        CommandLabel::MoveClips
    }

    fn apply(&mut self, project: &mut Project) {
        self.old_starts.clear();
        for &(clip_id, from_track, to_track, new_start) in &self.moves {
            let tl = &mut project.timelines[self.timeline];
            let Some(mut clip) = tl.tracks[from_track].remove_clip(clip_id) else {
                self.old_starts.push(None);
                continue;
            };
            self.old_starts.push(Some(clip.timeline_start));
            clip.timeline_start = new_start;
            tl.tracks[to_track].insert_sorted(clip);
        }
    }

    fn undo(&self, project: &mut Project) {
        for (&(clip_id, from_track, to_track, _new_start), old_start) in
            self.moves.iter().zip(&self.old_starts)
        {
            let Some(old_start) = old_start else {
                continue;
            };
            let tl = &mut project.timelines[self.timeline];
            let Some(mut clip) = tl.tracks[to_track].remove_clip(clip_id) else {
                continue;
            };
            clip.timeline_start = *old_start;
            tl.tracks[from_track].insert_sorted(clip);
        }
    }
}

/// Quale bordo di una clip viene trimmato: `Start` sposta l'inizio
/// insieme al contenuto (la fine sulla timeline resta ferma), `End` sposta
/// solo la fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimEdge {
    Start,
    End,
}

/// Trim di un bordo di una clip. Nessun clamp né collision-avoidance: il
/// chiamante calcola `new_value`.
#[derive(Debug)]
pub struct TrimClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub edge: TrimEdge,
    /// Nuova posizione di timeline del bordo.
    pub new_value: FrameIdx,
    /// `(timeline_start, source_offset, timeline_len)` precedenti.
    old: Option<(FrameIdx, FrameIdx, FrameIdx)>,
}

impl TrimClip {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        edge: TrimEdge,
        new_value: FrameIdx,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            edge,
            new_value,
            old: None,
        }
    }
}

impl Command for TrimClip {
    fn label(&self) -> CommandLabel {
        CommandLabel::TrimClips
    }

    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clip_mut(self.clip_id) else {
            return;
        };
        self.old = Some((clip.timeline_start, clip.source_offset, clip.timeline_len));
        match self.edge {
            TrimEdge::Start => {
                let delta = self.new_value - clip.timeline_start;
                clip.timeline_start = self.new_value;
                clip.source_offset += delta;
                clip.timeline_len -= delta;
            }
            TrimEdge::End => {
                clip.timeline_len = self.new_value - clip.timeline_start;
            }
        }
        debug_assert!(clip.timeline_len >= 1 && clip.source_offset >= 0, "trim fuori dai limiti");
        resort(track);
    }

    fn undo(&self, project: &mut Project) {
        let Some((timeline_start, source_offset, timeline_len)) = self.old else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clip_mut(self.clip_id) {
            clip.timeline_start = timeline_start;
            clip.source_offset = source_offset;
            clip.timeline_len = timeline_len;
        }
        resort(track);
    }
}

/// Quale dissolvenza tocca un `SetClipFade`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeEdge {
    In,
    Out,
}

/// Imposta la durata (in frame di timeline) della dissolvenza in entrata o
/// uscita di una clip, trascinata dall'handle in timeline.
#[derive(Debug)]
pub struct SetClipFade {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub edge: FadeEdge,
    pub new_value: FrameIdx,
    old: Option<FrameIdx>,
}

impl SetClipFade {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        new_value: FrameIdx,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            edge,
            new_value,
            old: None,
        }
    }

    fn field(edge: FadeEdge, clip: &mut Clip) -> &mut FrameIdx {
        match edge {
            FadeEdge::In => &mut clip.fade_in,
            FadeEdge::Out => &mut clip.fade_out,
        }
    }
}

impl Command for SetClipFade {
    fn label(&self) -> CommandLabel {
        CommandLabel::Fade
    }

    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clip_mut(self.clip_id) else {
            return;
        };
        let clamped = self.new_value.clamp(0, clip.timeline_len);
        let field = Self::field(self.edge, clip);
        self.old = Some(*field);
        *field = clamped;
    }

    fn undo(&self, project: &mut Project) {
        let Some(old) = self.old else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clip_mut(self.clip_id) {
            *Self::field(self.edge, clip) = old;
        }
    }
}

/// Scioglie l'intero gruppo collegato della clip, non solo lei.
#[derive(Debug)]
pub struct UnlinkClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    /// Gruppo sciolto e suoi membri, per l'undo.
    dissolved: Option<(LinkGroupId, Vec<(usize, ClipId)>)>,
}

impl UnlinkClip {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            dissolved: None,
        }
    }
}

impl Command for UnlinkClip {
    fn label(&self) -> CommandLabel {
        CommandLabel::UnlinkClips
    }

    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        let Some(group) = tl.clip(self.track_index, self.clip_id).and_then(|c| c.linked_group)
        else {
            return;
        };
        let members = tl.clips_in_group(group);
        for &(track_index, clip_id) in &members {
            if let Some(c) = tl.clip_mut(track_index, clip_id) {
                c.linked_group = None;
            }
        }
        self.dissolved = Some((group, members));
    }

    fn undo(&self, project: &mut Project) {
        let Some((group, members)) = &self.dissolved else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        for &(track_index, clip_id) in members {
            if let Some(c) = tl.clip_mut(track_index, clip_id) {
                c.linked_group = Some(*group);
            }
        }
    }
}

/// Collega le clip in un gruppo nuovo, sovrascrivendo quelli precedenti.
/// No-op con meno di 2 clip.
#[derive(Debug)]
pub struct LinkClips {
    pub timeline: TimelineId,
    pub targets: Vec<(usize, ClipId)>,
    /// Allocato la prima volta che `apply` gira, poi riusato invariato sui
    /// redo successivi — mai un nuovo id ad ogni redo.
    group_id: Option<LinkGroupId>,
    previous: Option<Vec<Option<LinkGroupId>>>,
}

impl LinkClips {
    pub fn new(timeline: TimelineId, targets: Vec<(usize, ClipId)>) -> Self {
        Self {
            timeline,
            targets,
            group_id: None,
            previous: None,
        }
    }
}

impl Command for LinkClips {
    fn label(&self) -> CommandLabel {
        CommandLabel::LinkClips
    }

    fn apply(&mut self, project: &mut Project) {
        if self.targets.len() < 2 {
            return;
        }
        let group_id = self
            .group_id
            .unwrap_or_else(|| project.alloc_link_group_id());
        self.group_id = Some(group_id);

        let tl = &project.timelines[self.timeline];
        let previous: Vec<Option<LinkGroupId>> = self
            .targets
            .iter()
            .map(|(track_index, clip_id)| {
                tl.clip(*track_index, *clip_id).and_then(|c| c.linked_group)
            })
            .collect();
        self.previous = Some(previous);

        let tl = &mut project.timelines[self.timeline];
        for (track_index, clip_id) in &self.targets {
            if let Some(c) = tl.clip_mut(*track_index, *clip_id) {
                c.linked_group = Some(group_id);
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        let Some(previous) = &self.previous else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        for ((track_index, clip_id), old) in self.targets.iter().zip(previous.iter()) {
            if let Some(c) = tl.clip_mut(*track_index, *clip_id) {
                c.linked_group = *old;
            }
        }
    }
}

/// Divide una clip a `split_at`; la metà destra ha un id nuovo. Su una clip
/// conformata le due metà possono condividere il frame sorgente a cavallo
/// del taglio.
#[derive(Debug)]
pub struct SplitClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub split_at: FrameIdx,
    original_len: Option<FrameIdx>,
    original_effects: Option<EffectStack>,
    new_clip_id: Option<ClipId>,
    /// Id della metà destra deciso dal chiamante, per ricollegare le metà di
    /// più split con un comando successivo.
    preallocated_new_clip_id: Option<ClipId>,
    /// Crossing transition tolte da `clip_id` durante lo split (di
    /// entrambe le metà nessuna resta la controparte geometrica attesa
    /// dalla transizione), da restituire su undo.
    removed_crossings: Vec<CrossTransition>,
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
            original_len: None,
            original_effects: None,
            new_clip_id: None,
            preallocated_new_clip_id: None,
            removed_crossings: Vec::new(),
        }
    }

    pub fn with_new_clip_id(mut self, id: ClipId) -> Self {
        self.preallocated_new_clip_id = Some(id);
        self
    }

    /// L'id assegnato alla metà destra, noto solo dopo `apply`.
    pub fn new_clip_id(&self) -> Option<ClipId> {
        self.new_clip_id
    }
}

impl Command for SplitClip {
    fn label(&self) -> CommandLabel {
        CommandLabel::SplitClips
    }

    fn apply(&mut self, project: &mut Project) {
        let new_id = self
            .preallocated_new_clip_id
            .unwrap_or_else(|| project.alloc_clip_id());
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clip_mut(self.clip_id) else {
            return;
        };
        if self.split_at <= clip.timeline_start || self.split_at >= clip.timeline_end() {
            return; // fuori dal corpo della clip: niente da dividere
        }

        self.original_len = Some(clip.timeline_len);
        let mut second_half = clip.clone();
        let left_len = self.split_at - clip.timeline_start;
        clip.timeline_len = left_len;
        // La metà sinistra è la stessa clip e resta nel suo gruppo; la destra è
        // nuova e parte scollegata.
        second_half.id = new_id;
        second_half.timeline_start = self.split_at;
        second_half.source_offset += left_len;
        second_half.timeline_len -= left_len;
        second_half.linked_group = None;
        self.new_clip_id = Some(new_id);

        // Ogni metà si tiene solo i keyframe del proprio lato del taglio:
        // altrimenti la destra interpolerebbe dai keyframe della sinistra.
        self.original_effects = Some(clip.effects.clone());
        clip.effects.drop_keyframes_from(clip.source_out());
        second_half.effects.drop_keyframes_before(second_half.source_in());

        track.insert_sorted(second_half);

        // Una crossing su `clip_id` puntava a un bordo che ora appartiene a
        // una delle due metà ma non più alla clip intera: invece di lasciarla
        // pendente (vedi doc di `Track::crossings`) la si toglie qui, valido
        // per qualunque tipo di crossing transition presente o futuro.
        self.removed_crossings = track.take_crossings_for(self.clip_id);
    }

    fn undo(&self, project: &mut Project) {
        let (Some(original_len), Some(new_clip_id)) = (self.original_len, self.new_clip_id)
        else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        track.clips.retain(|c| c.id != new_clip_id);
        if let Some(clip) = track.clip_mut(self.clip_id) {
            clip.timeline_len = original_len;
            if let Some(effects) = &self.original_effects {
                clip.effects = effects.clone();
            }
        }
        track.crossings.extend(self.removed_crossings.iter().cloned());
    }
}

/// Sostituisce un valore di una clip (scelto da `access`) ricordando il
/// precedente per l'undo.
pub struct SetClipValue<T> {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    value: T,
    access: Box<dyn Fn(&mut Clip) -> &mut T>,
    old: Option<T>,
    label: CommandLabel,
}

impl<T> SetClipValue<T> {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        label: CommandLabel,
        value: T,
        access: impl Fn(&mut Clip) -> &mut T + 'static,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            value,
            access: Box::new(access),
            old: None,
            label,
        }
    }
}

impl<T> std::fmt::Debug for SetClipValue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetClipValue")
            .field("track_index", &self.track_index)
            .field("clip_id", &self.clip_id)
            .finish_non_exhaustive()
    }
}

impl<T: Clone> Command for SetClipValue<T> {
    fn label(&self) -> CommandLabel {
        self.label
    }

    fn apply(&mut self, project: &mut Project) {
        if let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) {
            self.old = Some(std::mem::replace((self.access)(clip), self.value.clone()));
        }
    }

    fn undo(&self, project: &mut Project) {
        if let (Some(old), Some(clip)) = (
            &self.old,
            project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id),
        ) {
            *(self.access)(clip) = old.clone();
        }
    }
}

/// Valore statico (il `default`) di un parametro del transform.
pub fn set_clip_transform_param(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    param: TransformParam,
    value: f32,
) -> SetClipValue<f32> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Transform, value, move |c| {
        &mut c.effects.transform.track_mut(param).default
    })
}

pub fn set_clip_flip(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    value: [bool; 2],
) -> SetClipValue<[bool; 2]> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Flip, value, |c| &mut c.effects.transform.flip)
}

/// Gain statico (dB).
pub fn set_clip_gain(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    value: f32,
) -> SetClipValue<f32> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Gain, value, |c| &mut c.effects.gain_db.default)
}

/// Gain a 0 dB, keyframe compresi.
pub fn reset_clip_gain(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
) -> SetClipValue<Keyframed<f32>> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::ResetGain, Keyframed::constant(0.0), |c| {
        &mut c.effects.gain_db
    })
}

pub fn set_clip_title(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    value: TitleParams,
) -> SetClipValue<Option<TitleParams>> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Title, Some(value), |c| &mut c.effects.title)
}

/// Sostituisce l'intera lista filtri, ordine incluso: aggiungerne uno,
/// toglierlo, attivarlo/disattivarlo o riordinarli sono tutti "scrivi la
/// nuova lista" — chi chiama la calcola da quella attuale.
pub fn set_clip_filters(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    value: Vec<ClipFilter>,
) -> SetClipValue<Vec<ClipFilter>> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Filters, value, |c| {
        &mut c.effects.filters
    })
}

/// Imposta (o rimuove, con `None`) la transizione di un bordo della clip.
pub fn set_clip_transition(
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    edge: FadeEdge,
    value: Option<Transition>,
) -> SetClipValue<Option<Transition>> {
    SetClipValue::new(timeline, track_index, clip_id, CommandLabel::Transition, value, move |c| {
        match edge {
            FadeEdge::In => &mut c.effects.transition_in,
            FadeEdge::Out => &mut c.effects.transition_out,
        }
    })
}

/// Aggiunge, sostituisce o rimuove (`value: None`) la crossing transition il
/// cui `left_clip` è `left_clip`: una clip ha al più una transizione sul suo
/// bordo destro, quindi la identifica da sola, senza bisogno dell'id di
/// `right_clip`.
#[derive(Debug)]
pub struct SetCrossTransition {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub left_clip: ClipId,
    pub value: Option<CrossTransition>,
    /// `None` finché non applicato; poi il valore precedente (che a sua
    /// volta può essere `None` se non c'era nessuna crossing lì).
    old: Option<Option<CrossTransition>>,
}

impl SetCrossTransition {
    pub fn new(timeline: TimelineId, track_index: usize, left_clip: ClipId, value: Option<CrossTransition>) -> Self {
        Self { timeline, track_index, left_clip, value, old: None }
    }

    fn write(track: &mut Track, left_clip: ClipId, value: &Option<CrossTransition>) -> Option<CrossTransition> {
        let pos = track.crossings.iter().position(|c| c.left_clip == left_clip);
        let old = pos.map(|i| track.crossings[i].clone());
        match (pos, value) {
            (Some(i), Some(new)) => track.crossings[i] = new.clone(),
            (Some(i), None) => {
                track.crossings.remove(i);
            }
            (None, Some(new)) => track.crossings.push(new.clone()),
            (None, None) => {}
        }
        old
    }
}

impl Command for SetCrossTransition {
    fn label(&self) -> CommandLabel {
        CommandLabel::Transition
    }

    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        self.old = Some(Self::write(track, self.left_clip, &self.value));
    }

    fn undo(&self, project: &mut Project) {
        let Some(old) = &self.old else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        Self::write(track, self.left_clip, old);
    }
}

/// Riporta un gruppo di parametri del transform al valore di default,
/// keyframe compresi: è il reset di una sezione del pannello proprietà
/// (Transform o Cropping), non di un parametro singolo.
#[derive(Debug)]
pub struct ResetTransformParams {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub params: Vec<TransformParam>,
    /// `true` per il gruppo Transform, che comprende anche il flip.
    pub reset_flip: bool,
    previous: Option<(Vec<Keyframed<f32>>, [bool; 2])>,
}

impl ResetTransformParams {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        params: Vec<TransformParam>,
        reset_flip: bool,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            params,
            reset_flip,
            previous: None,
        }
    }
}

impl Command for ResetTransformParams {
    fn label(&self) -> CommandLabel {
        CommandLabel::ResetTransform
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        let defaults = Transform::default();
        let previous = self
            .params
            .iter()
            .map(|p| clip.effects.transform.track(*p).clone())
            .collect();
        self.previous = Some((previous, clip.effects.transform.flip));
        for param in &self.params {
            *clip.effects.transform.track_mut(*param) = Keyframed::constant(param.of(&defaults));
        }
        if self.reset_flip {
            clip.effects.transform.flip = defaults.flip;
        }
    }

    fn undo(&self, project: &mut Project) {
        let Some((previous, flip)) = &self.previous else {
            return;
        };
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        for (param, track) in self.params.iter().zip(previous) {
            *clip.effects.transform.track_mut(*param) = track.clone();
        }
        if self.reset_flip {
            clip.effects.transform.flip = *flip;
        }
    }
}

/// Colore statico di una clip SolidColor; il primo lo inizializza. I
/// comandi keyframe presuppongono un colore già presente.
#[derive(Debug)]
pub struct SetClipColor {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub new_value: Rgba,
    old_value: Option<Option<Rgba>>,
}

impl SetClipColor {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId, new_value: Rgba) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            new_value,
            old_value: None,
        }
    }
}

impl Command for SetClipColor {
    fn label(&self) -> CommandLabel {
        CommandLabel::ClipColor
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        self.old_value = Some(clip.effects.color.as_ref().map(|k| k.default));
        match &mut clip.effects.color {
            Some(k) => k.default = self.new_value,
            None => clip.effects.color = Some(Keyframed::constant(self.new_value)),
        }
    }

    fn undo(&self, project: &mut Project) {
        let Some(old) = self.old_value else {
            return;
        };
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        match old {
            Some(v) => {
                if let Some(k) = &mut clip.effects.color {
                    k.default = v;
                }
            }
            None => clip.effects.color = None,
        }
    }
}

/// Parametro animabile di `UpsertKeyframe`/`RemoveKeyframe`.
#[derive(Debug, Clone, Copy)]
pub enum KeyframeValue {
    TransformParam(TransformParam, f32),
    Gain(f32),
    Color(Rgba),
}

/// Inserisce o sostituisce un keyframe di un parametro animabile.
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
    fn label(&self) -> CommandLabel {
        CommandLabel::SetKeyframe
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        match self.value {
            KeyframeValue::TransformParam(param, v) => {
                let track = clip.effects.transform.track_mut(param);
                self.previous = track
                    .keyframe_at(self.frame)
                    .map(|(v, i)| (KeyframeValue::TransformParam(param, v), i));
                track.upsert(self.frame, v, self.interpolation);
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
            KeyframeValue::Color(v) => {
                let Some(color) = &mut clip.effects.color else {
                    return; // nessun colore inizializzato: vedi doc di SetClipColor
                };
                self.previous = color
                    .keyframe_at(self.frame)
                    .map(|(v, i)| (KeyframeValue::Color(v), i));
                color.upsert(self.frame, v, self.interpolation);
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        match &self.previous {
            Some((KeyframeValue::TransformParam(param, v), i)) => {
                clip.effects
                    .transform
                    .track_mut(*param)
                    .upsert(self.frame, *v, *i);
            }
            Some((KeyframeValue::Gain(v), i)) => {
                clip.effects.gain_db.upsert(self.frame, *v, *i);
            }
            Some((KeyframeValue::Color(v), i)) => {
                if let Some(color) = &mut clip.effects.color {
                    color.upsert(self.frame, *v, *i);
                }
            }
            None => match self.value {
                KeyframeValue::TransformParam(param, _) => {
                    clip.effects.transform.track_mut(param).remove_at(self.frame);
                }
                KeyframeValue::Gain(_) => {
                    clip.effects.gain_db.remove_at(self.frame);
                }
                KeyframeValue::Color(_) => {
                    if let Some(color) = &mut clip.effects.color {
                        color.remove_at(self.frame);
                    }
                }
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum KeyframeTarget {
    TransformParam(TransformParam),
    Gain,
    Color,
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
    fn label(&self) -> CommandLabel {
        CommandLabel::RemoveKeyframe
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        self.removed = match self.target {
            KeyframeTarget::TransformParam(param) => clip
                .effects
                .transform
                .track_mut(param)
                .remove_at(self.frame)
                .map(|(v, i)| (KeyframeValue::TransformParam(param, v), i)),
            KeyframeTarget::Gain => clip
                .effects
                .gain_db
                .remove_at(self.frame)
                .map(|(v, i)| (KeyframeValue::Gain(v), i)),
            KeyframeTarget::Color => clip
                .effects
                .color
                .as_mut()
                .and_then(|c| c.remove_at(self.frame))
                .map(|(v, i)| (KeyframeValue::Color(v), i)),
        };
    }

    fn undo(&self, project: &mut Project) {
        let Some((value, interp)) = self.removed else {
            return;
        };
        let Some(clip) = project.timelines[self.timeline].clip_mut(self.track_index, self.clip_id) else {
            return;
        };
        match value {
            KeyframeValue::TransformParam(param, v) => clip
                .effects
                .transform
                .track_mut(param)
                .upsert(self.frame, v, interp),
            KeyframeValue::Gain(v) => clip.effects.gain_db.upsert(self.frame, v, interp),
            KeyframeValue::Color(v) => {
                if let Some(color) = &mut clip.effects.color {
                    color.upsert(self.frame, v, interp);
                }
            }
        }
    }
}

/// Rimuove un media dal pool; le sue clip restano offline. `slotmap` non
/// reinserisce con la stessa chiave: l'undo riscrive le clip col nuovo id,
/// da cui `Cell`/`RefCell` (`undo` prende `&self`).
#[derive(Debug)]
pub struct RemoveMedia {
    media: Cell<MediaId>,
    removed: RefCell<Option<MediaItem>>,
}

impl RemoveMedia {
    pub fn new(media: MediaId) -> Self {
        Self {
            media: Cell::new(media),
            removed: RefCell::new(None),
        }
    }
}

impl Command for RemoveMedia {
    fn label(&self) -> CommandLabel {
        CommandLabel::RemoveMedia
    }

    fn apply(&mut self, project: &mut Project) {
        *self.removed.borrow_mut() = project.media_pool.remove(self.media.get());
    }

    fn undo(&self, project: &mut Project) {
        let Some(item) = self.removed.borrow_mut().take() else {
            return;
        };
        let old = self.media.get();
        let new = project.media_pool.insert(item);
        self.media.set(new);
        for timeline in project.timelines.values_mut() {
            for track in &mut timeline.tracks {
                for clip in &mut track.clips {
                    if matches!(clip.source, ClipSource::Media(id) if id == old) {
                        clip.source = ClipSource::Media(new);
                    }
                }
            }
        }
    }
}

/// Relink: nuovo percorso (e `content_hash`) per un media del pool.
#[derive(Debug)]
pub struct SetMediaPath {
    media: MediaId,
    new_path: PathBuf,
    new_content_hash: u64,
    old: RefCell<Option<(PathBuf, u64)>>,
}

impl SetMediaPath {
    pub fn new(media: MediaId, new_path: PathBuf, new_content_hash: u64) -> Self {
        Self {
            media,
            new_path,
            new_content_hash,
            old: RefCell::new(None),
        }
    }
}

impl Command for SetMediaPath {
    fn label(&self) -> CommandLabel {
        CommandLabel::RelinkMedia
    }

    fn apply(&mut self, project: &mut Project) {
        let Some(item) = project.media_pool.get_mut(self.media) else {
            return;
        };
        *self.old.borrow_mut() = Some((item.path.clone(), item.content_hash));
        item.path = self.new_path.clone();
        item.content_hash = self.new_content_hash;
    }

    fn undo(&self, project: &mut Project) {
        let Some((path, hash)) = self.old.borrow_mut().take() else {
            return;
        };
        if let Some(item) = project.media_pool.get_mut(self.media) {
            item.path = path;
            item.content_hash = hash;
        }
    }
}

/// `(timeline_start, timeline_end)` di una clip, se esiste.
fn clip_bounds(
    project: &Project,
    timeline_id: TimelineId,
    track_index: usize,
    clip_id: ClipId,
) -> Option<(FrameIdx, FrameIdx)> {
    let clip = project.timelines[timeline_id].clip(track_index, clip_id)?;
    Some((clip.timeline_start, clip.timeline_end()))
}

/// Accoda i comandi che liberano `[new_start, new_end)` da una clip che lo
/// invade: rimossa se coperta, accorciata se sporge da un lato, divisa se
/// il tratto cade nel mezzo (restituisce la metà destra).
#[allow(clippy::too_many_arguments)]
fn resolve_overlap(
    project: &mut Project,
    timeline_id: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    old_start: FrameIdx,
    old_end: FrameIdx,
    new_start: FrameIdx,
    new_end: FrameIdx,
    commands: &mut Vec<Box<dyn Command>>,
) -> Option<ClipId> {
    if old_start >= new_start && old_end <= new_end {
        commands.push(Box::new(LiftDelete::new(
            timeline_id,
            track_index,
            clip_id,
        )));
        None
    } else if old_start < new_start && old_end > new_end {
        // Il nuovo intervallo cade nel mezzo: divide la clip in due,
        // poi accorcia la metà destra dal suo bordo sinistro fino a
        // `new_end`.
        let right_id = project.alloc_clip_id();
        commands.push(Box::new(
            SplitClip::new(timeline_id, track_index, clip_id, new_start)
                .with_new_clip_id(right_id),
        ));
        commands.push(Box::new(TrimClip::new(
            timeline_id,
            track_index,
            right_id,
            TrimEdge::Start,
            new_end,
        )));
        Some(right_id)
    } else if old_start < new_start {
        // La coda sporge oltre `new_start`: accorcia il bordo destro
        // (fine) fin lì.
        commands.push(Box::new(TrimClip::new(
            timeline_id,
            track_index,
            clip_id,
            TrimEdge::End,
            new_start,
        )));
        None
    } else {
        // La testa sporge prima di `new_end`: accorcia il bordo
        // sinistro (inizio) fin lì.
        commands.push(Box::new(TrimClip::new(
            timeline_id,
            track_index,
            clip_id,
            TrimEdge::Start,
            new_end,
        )));
        None
    }
}

/// Accoda i comandi che liberano i tratti `ranges` (track, inizio, fine):
/// le clip sotto vengono accorciate, divise o rimosse, mai lasciate
/// sovrapposte. Se un taglio divide più membri di uno stesso gruppo tra le
/// track coinvolte, le loro metà destre vengono ricollegate.
pub fn make_room_for_ranges(
    project: &mut Project,
    timeline_id: TimelineId,
    ranges: &[(usize, FrameIdx, FrameIdx)],
    exclude: &[(usize, ClipId)],
    commands: &mut Vec<Box<dyn Command>>,
) {
    let mut processed: BTreeSet<(usize, ClipId)> = exclude.iter().copied().collect();
    let range_tracks: BTreeSet<usize> = ranges.iter().map(|(t, _, _)| *t).collect();

    for &(track_index, new_start, new_end) in ranges {
        if new_start >= new_end {
            continue;
        }
        type Overlapping = (
            ClipId,
            FrameIdx,
            FrameIdx,
            Option<LinkGroupId>,
        );
        let overlapping: Vec<Overlapping> =
            project.timelines[timeline_id]
                .tracks
                .get(track_index)
                .map(|t| {
                    t.clips
                        .iter()
                        .filter(|c| c.timeline_start < new_end && c.timeline_end() > new_start)
                        .map(|c| {
                            (
                                c.id,
                                c.timeline_start,
                                c.timeline_end(),
                                c.linked_group,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

        for (clip_id, old_start, old_end, group) in overlapping {
            if !processed.insert((track_index, clip_id)) {
                continue;
            }
            let split_halves = resolve_overlap(project, 
                timeline_id,
                track_index,
                clip_id,
                old_start,
                old_end,
                new_start,
                new_end,
                commands,
            );

            if group.is_none() {
                continue;
            }
            let mut new_rights: Vec<(usize, ClipId)> =
                split_halves.map(|right| (track_index, right)).into_iter().collect();

            for (member_track, member_id) in project.timelines[timeline_id].linked_members(track_index, clip_id) {
                if !range_tracks.contains(&member_track)
                    || !processed.insert((member_track, member_id))
                {
                    continue;
                }
                let Some((m_start, m_end)) =
                    clip_bounds(project, timeline_id, member_track, member_id)
                else {
                    continue;
                };
                let member_split = resolve_overlap(project, 
                    timeline_id,
                    member_track,
                    member_id,
                    m_start,
                    m_end,
                    new_start,
                    new_end,
                    commands,
                );
                if let Some(right) = member_split {
                    new_rights.push((member_track, right));
                }
            }

            if new_rights.len() >= 2 {
                commands.push(Box::new(LinkClips::new(timeline_id, new_rights)));
            }
        }
    }
}


/// Comandi che inseriscono `clips` (track, clip, gruppo) sovrascrivendo
/// quel che c'è sotto; le clip con la stessa chiave di gruppo finiscono in
/// un gruppo collegato nuovo.
pub fn insert_overwriting<K: PartialEq>(
    project: &mut Project,
    timeline_id: TimelineId,
    clips: Vec<(usize, Clip, Option<K>)>,
) -> Vec<Box<dyn Command>> {
    let ranges: Vec<(usize, FrameIdx, FrameIdx)> = clips
        .iter()
        .map(|(track, clip, _)| (*track, clip.timeline_start, clip.timeline_end()))
        .collect();
    let mut commands: Vec<Box<dyn Command>> = Vec::new();
    make_room_for_ranges(project, timeline_id, &ranges, &[], &mut commands);
    let mut groups: Vec<(K, Vec<(usize, ClipId)>)> = Vec::new();
    for (track_index, clip, key) in clips {
        if let Some(key) = key {
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push((track_index, clip.id)),
                None => groups.push((key, vec![(track_index, clip.id)])),
            }
        }
        commands.push(Box::new(InsertClip {
            timeline: timeline_id,
            track_index,
            clip,
        }));
    }
    for (_, members) in groups {
        if members.len() >= 2 {
            commands.push(Box::new(LinkClips::new(timeline_id, members)));
        }
    }
    commands
}

/// Accoda i tagli per le sovrapposizioni rimaste: vince la clip che
/// comincia dopo. Rete di sicurezza dopo gli spostamenti in blocco.
pub fn cut_overlaps(
    project: &mut Project,
    timeline_id: TimelineId,
    commands: &mut Vec<Box<dyn Command>>,
) {
    for track_index in 0..project.timelines[timeline_id].tracks.len() {
        if project.timelines[timeline_id].is_locked(track_index) {
            continue;
        }
        let clips: Vec<(ClipId, FrameIdx, FrameIdx)> = project.timelines[timeline_id].tracks
            [track_index]
            .clips
            .iter()
            .map(|c| (c.id, c.timeline_start, c.timeline_end()))
            .collect();
        for (i, &(clip_id, start, end)) in clips.iter().enumerate() {
            let Some(&(_, cut_at, _)) = clips[i + 1..]
                .iter()
                .filter(|&&(_, other_start, _)| other_start > start && other_start < end)
                .min_by_key(|&&(_, other_start, _)| other_start)
            else {
                // Nessuna clip che comincia dentro questa: o non si
                // sovrappone a nulla, o è lei quella coperta del tutto.
                if clips[i + 1..]
                    .iter()
                    .any(|&(_, other_start, other_end)| {
                        other_start <= start && other_end >= end
                    })
                {
                    commands.push(Box::new(LiftDelete::new(timeline_id, track_index, clip_id)));
                }
                continue;
            };
            commands.push(Box::new(TrimClip::new(
                timeline_id,
                track_index,
                clip_id,
                TrimEdge::End,
                cut_at,
            )));
        }
    }
}

