//! Command pattern per undo/redo. Ogni comando cattura da sé lo stato
//! necessario a invertirsi nel momento in cui viene applicato.

use crate::model::{
    Clip, ClipId, ClipSource, FrameIdx, Interpolation, Keyframed, LinkGroupId,
    MediaId, MediaItem, Project, Rgba, TimelineId, Track, TrackKind, Transform,
    TransformParam,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

pub trait Command: std::fmt::Debug {
    fn apply(&mut self, project: &mut Project);
    fn undo(&self, project: &mut Project);
}

/// Raggruppa più comandi in un solo passo di history: un solo undo li
/// riporta tutti indietro insieme. Usato per operazioni che toccano più
/// clip in una volta sola dal punto di vista dell'utente (es. "taglia al
/// playhead" su tutte le track).
#[derive(Debug)]
pub struct CompositeCommand {
    commands: Vec<Box<dyn Command>>,
}

impl CompositeCommand {
    pub fn new(commands: Vec<Box<dyn Command>>) -> Self {
        Self { commands }
    }
}

impl Command for CompositeCommand {
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

    /// Inizio di un gruppo: i comandi eseguiti da qui fino a `end_group`
    /// diventeranno un unico passo di undo. Serve quando una sola azione
    /// dell'utente si scompone in più comandi (es. il drop di più media,
    /// che inserisce una clip per stream audio e crea le track mancanti):
    /// senza, ogni pezzo richiederebbe il suo Ctrl+Z.
    pub fn begin_group(&mut self) -> GroupMark {
        GroupMark(self.undo_stack.len())
    }

    pub fn end_group(&mut self, mark: GroupMark) {
        let commands = self.undo_stack.split_off(mark.0.min(self.undo_stack.len()));
        if commands.len() > 1 {
            self.undo_stack
                .push(Box::new(CompositeCommand::new(commands)));
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

    /// Incrementato a ogni modifica effettiva del progetto
    /// (`do_command`/`undo`/`redo` che hanno davvero fatto qualcosa, non
    /// una `redo`/`undo` a vuoto su uno stack esaurito): permette di
    /// rilevare "il progetto è cambiato da quando l'ho controllato
    /// l'ultima volta" con un semplice confronto di interi, invece di
    /// dover clonare/diffare l'intero `Project` a ogni frame UI — usato
    /// da `VibeVideoApp` per sapere quando notificare il worker di
    /// render-ahead di una nuova disposizione delle clip.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Aggiunge una track vuota in coda a `tracks` (REFACTOR_PIPELINE.md B4).
/// Sempre in coda, mai in una posizione "intelligente" in base al tipo:
/// l'ordine dei `tracks` conta solo per il compositing video
/// (bottom->top, vedi `Timeline::active_video_clip_at`), e lì contano solo
/// le posizioni *relative* tra track Video — un'eventuale track Audio
/// interposta non cambia quale track Video è la più in alto. Appendere e
/// basta evita qualunque caso speciale su "dove va inserita".
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

/// Rimuove una track (con tutte le sue clip). Non fa da sé la verifica "non
/// è l'ultima del suo tipo": quella è una regola della UI (mantenere sempre
/// almeno una track Video e una Audio), non un invariante del modello dati
/// — un progetto caricato da un file esterno potrebbe legittimamente non
/// averne.
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

/// Ripple delete globale: rimuove la clip (ed eventuali altre indicate in
/// `also_remove`, tipicamente la gemella collegata) e chiude il gap su
/// *tutte* le track della Timeline, mantenendo il sync audio/video. Le
/// dimensioni del gap sono sempre quelle della clip primaria: le clip in
/// `also_remove` vengono semplicemente rimosse (non shiftate) insieme a
/// lei, non allargano il gap.
#[derive(Debug)]
struct RippleState {
    // (track_index, clip) per ciascuna clip rimossa (primaria + also_remove).
    removed: Vec<(usize, Clip)>,
    // Posizione originale (prima dello shift) di ogni clip *non* rimossa ma
    // toccata su ogni track, per un undo esatto: non si può ricostruire lo
    // shift da una sola soglia perché, dopo aver sottratto `gap_len`, un
    // valore shiftato può risultare identico o minore di uno non toccato
    // (vedi test).
    shifted: Vec<(usize, ClipId, FrameIdx)>, // (track_index, clip_id, original_start)
}

#[derive(Debug)]
pub struct RippleDeleteAllTracks {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub also_remove: Vec<(usize, ClipId)>,
    removed: Option<RippleState>,
}

impl RippleDeleteAllTracks {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            also_remove: Vec::new(),
            removed: None,
        }
    }

    /// Altre clip da rimuovere insieme (tipicamente la gemella collegata
    /// della clip primaria), come parte dello stesso "buco".
    pub fn with_also_remove(mut self, also_remove: Vec<(usize, ClipId)>) -> Self {
        self.also_remove = also_remove;
        self
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
        let primary = track.clips.remove(pos);
        let gap_start = primary.timeline_start;
        let gap_len = primary.timeline_len;

        let mut removed = vec![(self.track_index, primary)];
        for &(also_track, also_id) in &self.also_remove {
            if let Some(p) = tl.tracks[also_track]
                .clips
                .iter()
                .position(|c| c.id == also_id)
            {
                removed.push((also_track, tl.tracks[also_track].clips.remove(p)));
            }
        }

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
        self.removed = Some(RippleState { removed, shifted });
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
        for (track_index, clip) in &state.removed {
            tl.tracks[*track_index].clips.push(clip.clone());
        }
        for track in &mut tl.tracks {
            resort(track);
        }
    }
}

/// Chiude uno spazio vuoto (nessuna clip da rimuovere) su tutte le track,
/// shiftando indietro di `gap_len` ogni clip che inizia a `gap_start` o
/// dopo — stessa identica meccanica di shift di `RippleDeleteAllTracks`
/// (mantiene il sync A/V globale), ma senza rimuovere alcuna clip: usato
/// per il ripple delete su un vuoto selezionato (comportamento "seleziona
/// il vuoto, ripple delete" di DaVinci Resolve).
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
    fn apply(&mut self, project: &mut Project) {
        let tl = &mut project.timelines[self.timeline];
        let mut shifted = Vec::new();
        for (track_index, track) in tl.tracks.iter_mut().enumerate() {
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
            let track = &mut tl.tracks[*track_index];
            if let Some(c) = track.clips.iter_mut().find(|c| c.id == *clip_id) {
                c.timeline_start = *original_start;
            }
        }
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

/// Sposta più clip in un unico passo di history (un solo undo le riporta
/// indietro tutte insieme): serve per trascinare la selezione corrente,
/// che contiene sempre un gruppo collegato per intero se ce n'è uno
/// (`Clip::linked_group`, tipicamente audio+video della stessa sorgente),
/// che deve muoversi come una singola unità agli occhi dell'utente.
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
    fn apply(&mut self, project: &mut Project) {
        self.old_starts.clear();
        for &(clip_id, from_track, to_track, new_start) in &self.moves {
            let tl = &mut project.timelines[self.timeline];
            let from = &mut tl.tracks[from_track];
            let Some(pos) = from.clips.iter().position(|c| c.id == clip_id) else {
                self.old_starts.push(None);
                continue;
            };
            let mut clip = from.clips.remove(pos);
            self.old_starts.push(Some(clip.timeline_start));
            clip.timeline_start = new_start;

            let to = &mut tl.tracks[to_track];
            let insert_at = to
                .clips
                .partition_point(|c| c.timeline_start < clip.timeline_start);
            to.clips.insert(insert_at, clip);
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
            let to = &mut tl.tracks[to_track];
            let Some(pos) = to.clips.iter().position(|c| c.id == clip_id) else {
                continue;
            };
            let mut clip = to.clips.remove(pos);
            clip.timeline_start = *old_start;

            let from = &mut tl.tracks[from_track];
            let insert_at = from
                .clips
                .partition_point(|c| c.timeline_start < clip.timeline_start);
            from.clips.insert(insert_at, clip);
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

/// Trim di un bordo di una clip (drag di una maniglia sul bordo sinistro/
/// destro nella timeline), distinto da un semplice spostamento
/// (`MoveClip`): cambia la *durata* della clip, non solo la sua
/// posizione. Come `MoveClip`, non fa collision-avoidance né clamp ai
/// limiti del sorgente da sé: il chiamante calcola `new_value` prima di
/// emettere il comando.
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
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.timeline_start = timeline_start;
            clip.source_offset = source_offset;
            clip.timeline_len = timeline_len;
        }
        resort(track);
    }
}

/// Scioglie l'*intero gruppo collegato* (`Clip::linked_group`) di una clip,
/// se ne ha uno: ogni membro del gruppo esce, non solo la clip su cui è
/// stato invocato — "scollega" rompe il collegamento, non rimuove un
/// singolo membro da un gruppo che continuerebbe a esistere per gli altri
/// (comportamento segnalato come bug: cliccare "Scollega" su una clip di
/// un gruppo da 3 lasciava le altre due ancora collegate tra loro).
#[derive(Debug)]
pub struct UnlinkClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    /// Ogni clip del gruppo sciolto, col suo `linked_group` *precedente*
    /// (sempre lo stesso `Some(group)` per tutte), per l'undo.
    affected: Option<Vec<(usize, ClipId, Option<LinkGroupId>)>>,
}

impl UnlinkClip {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            affected: None,
        }
    }
}

impl Command for UnlinkClip {
    fn apply(&mut self, project: &mut Project) {
        let tl = &project.timelines[self.timeline];
        let Some(group) = tl
            .tracks
            .get(self.track_index)
            .and_then(|t| t.clips.iter().find(|c| c.id == self.clip_id))
            .and_then(|c| c.linked_group)
        else {
            return;
        };
        let affected: Vec<(usize, ClipId, Option<LinkGroupId>)> = tl
            .clips_in_group(group)
            .into_iter()
            .map(|(track_index, clip_id)| (track_index, clip_id, Some(group)))
            .collect();
        self.affected = Some(affected);

        let tl = &mut project.timelines[self.timeline];
        for (track_index, clip_id, _) in self.affected.as_ref().unwrap() {
            if let Some(c) = tl.tracks[*track_index].clips.iter_mut().find(|c| c.id == *clip_id) {
                c.linked_group = None;
            }
        }
    }

    fn undo(&self, project: &mut Project) {
        let Some(affected) = &self.affected else {
            return;
        };
        let tl = &mut project.timelines[self.timeline];
        for (track_index, clip_id, old_group) in affected {
            if let Some(c) = tl.tracks[*track_index].clips.iter_mut().find(|c| c.id == *clip_id) {
                c.linked_group = *old_group;
            }
        }
    }
}

/// Collega un insieme di clip nello stesso gruppo (`Clip::linked_group`):
/// selezione, drag e cancellazione trattano un gruppo come un'unità.
/// Qualunque gruppo precedente delle clip coinvolte viene sovrascritto (non
/// unito) — collegare è una scelta esplicita di un insieme esatto, non
/// un'unione di gruppi preesistenti. No-op se `targets` ha meno di 2 clip
/// (niente da collegare).
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
                tl.tracks
                    .get(*track_index)
                    .and_then(|t| t.clips.iter().find(|c| c.id == *clip_id))
                    .and_then(|c| c.linked_group)
            })
            .collect();
        self.previous = Some(previous);

        let tl = &mut project.timelines[self.timeline];
        for (track_index, clip_id) in &self.targets {
            if let Some(c) = tl
                .tracks
                .get_mut(*track_index)
                .and_then(|t| t.clips.iter_mut().find(|c| c.id == *clip_id))
            {
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
            if let Some(c) = tl
                .tracks
                .get_mut(*track_index)
                .and_then(|t| t.clips.iter_mut().find(|c| c.id == *clip_id))
            {
                c.linked_group = *old;
            }
        }
    }
}

/// Divide una clip in due al tempo di timeline `split_at`. La seconda metà
/// riceve un nuovo `ClipId`. Su una clip conformata (`Clip::rate`) le due
/// metà possono condividere il frame sorgente a cavallo del taglio,
/// mostrato per una parte del suo tempo a sinistra e per il resto a
/// destra.
#[derive(Debug)]
pub struct SplitClip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub split_at: FrameIdx,
    original_len: Option<FrameIdx>,
    new_clip_id: Option<ClipId>,
    /// Se impostato (`with_new_clip_id`), l'id della metà destra è questo
    /// invece di uno allocato al volo: serve a chi orchestra più split
    /// collegati (vedi `split_at_playhead`) per conoscere in anticipo
    /// gli id delle metà e poterle ricollegare con un comando successivo.
    preallocated_new_clip_id: Option<ClipId>,
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
            new_clip_id: None,
            preallocated_new_clip_id: None,
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
    fn apply(&mut self, project: &mut Project) {
        let new_id = self
            .preallocated_new_clip_id
            .unwrap_or_else(|| project.alloc_clip_id());
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        if self.split_at <= clip.timeline_start || self.split_at >= clip.timeline_end() {
            return; // fuori dal corpo della clip: niente da dividere
        }

        self.original_len = Some(clip.timeline_len);
        let mut second_half = clip.clone();
        let left_len = self.split_at - clip.timeline_start;
        clip.timeline_len = left_len;
        // `clip` (la metà sinistra) mantiene il suo `linked_group`
        // invariato: è la stessa clip di prima, solo accorciata, e resta
        // collegata a chiunque altro condivida quel gruppo (che sia stato
        // diviso insieme o no). La metà destra è una clip nuova, che parte
        // scollegata — chi orchestra più split insieme (`split_at_playhead`)
        // ricollega le metà destre tra loro con un `LinkClips` a parte.
        second_half.id = new_id;
        second_half.timeline_start = self.split_at;
        second_half.source_offset += left_len;
        second_half.timeline_len -= left_len;
        second_half.linked_group = None;
        self.new_clip_id = Some(new_id);

        let insert_at = track
            .clips
            .partition_point(|c| c.timeline_start < second_half.timeline_start);
        track.clips.insert(insert_at, second_half);
    }

    fn undo(&self, project: &mut Project) {
        let (Some(original_len), Some(new_clip_id)) = (self.original_len, self.new_clip_id)
        else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        track.clips.retain(|c| c.id != new_clip_id);
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.timeline_len = original_len;
        }
    }
}

/// Imposta il valore *statico* di un parametro del transform, cioè il
/// `default` del suo `Keyframed` (ogni parametro ha i suoi keyframe, vedi
/// `TransformTracks`).
#[derive(Debug)]
pub struct SetClipTransformParam {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub param: TransformParam,
    pub new_value: f32,
    old_value: Option<f32>,
}

impl SetClipTransformParam {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        param: TransformParam,
        new_value: f32,
    ) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            param,
            new_value,
            old_value: None,
        }
    }
}

impl Command for SetClipTransformParam {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        let track = clip.effects.transform.track_mut(self.param);
        self.old_value = Some(track.default);
        track.default = self.new_value;
    }

    fn undo(&self, project: &mut Project) {
        let Some(old_value) = self.old_value else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.effects.transform.track_mut(self.param).default = old_value;
        }
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
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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

/// Specchiatura della clip: non è un parametro animabile (nessun valore
/// intermedio tra specchiato e no), quindi ha il suo comando invece di
/// passare da `SetClipTransformParam`.
#[derive(Debug)]
pub struct SetClipFlip {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    pub new_value: [bool; 2],
    old_value: Option<[bool; 2]>,
}

impl SetClipFlip {
    pub fn new(
        timeline: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        new_value: [bool; 2],
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

impl Command for SetClipFlip {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        self.old_value = Some(clip.effects.transform.flip);
        clip.effects.transform.flip = self.new_value;
    }

    fn undo(&self, project: &mut Project) {
        let Some(old_value) = self.old_value else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.effects.transform.flip = old_value;
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

/// Riporta il gain di una clip al default (0 dB), keyframe compresi:
/// il "↺" della riga Volume nel pannello.
#[derive(Debug)]
pub struct ResetClipGain {
    pub timeline: TimelineId,
    pub track_index: usize,
    pub clip_id: ClipId,
    previous: Option<Keyframed<f32>>,
}

impl ResetClipGain {
    pub fn new(timeline: TimelineId, track_index: usize, clip_id: ClipId) -> Self {
        Self {
            timeline,
            track_index,
            clip_id,
            previous: None,
        }
    }
}

impl Command for ResetClipGain {
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
            return;
        };
        self.previous = Some(clip.effects.gain_db.clone());
        clip.effects.gain_db = Keyframed::constant(0.0);
    }

    fn undo(&self, project: &mut Project) {
        let Some(previous) = &self.previous else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.effects.gain_db = previous.clone();
        }
    }
}

/// Imposta il colore *statico* (`effects.color.default`) di una clip
/// SolidColor. A differenza di transform/gain, `effects.color` parte
/// `None`: il primo `SetClipColor` lo inizializza. I comandi keyframe
/// (`UpsertKeyframe`/`RemoveKeyframe`, sotto) assumono invece che sia già
/// inizializzato e non fanno nulla altrimenti — in pratica non è un
/// problema perché una clip SolidColor riceve sempre un colore alla
/// creazione (vedi vv-app).
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
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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

/// Il parametro animabile a cui si applica un `UpsertKeyframe`/
/// `RemoveKeyframe`. Comandi parametrizzati su un enum invece di uno per
/// parametro: quando arriveranno altri parametri keyframeable (es. speed)
/// basterà aggiungere una variante qui, non un'altra coppia di comandi.
#[derive(Debug, Clone, Copy)]
pub enum KeyframeValue {
    TransformParam(TransformParam, f32),
    Gain(f32),
    Color(Rgba),
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
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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
    fn apply(&mut self, project: &mut Project) {
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) else {
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

/// Rimuove un media dal media pool. Le clip che lo usano restano in
/// timeline e diventano "offline" (la loro `MediaId` non risolve più).
///
/// `slotmap` non permette di reinserire con la stessa chiave, quindi
/// l'undo ottiene una `MediaId` nuova e deve riscrivere i riferimenti in
/// tutte le clip; per lo stesso motivo lo stato (item rimosso e id
/// corrente) sta dietro a `Cell`/`RefCell`: `Command::undo` prende `&self`.
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

/// `(timeline_start, timeline_end)` di una clip, se esiste.
fn clip_bounds(
    project: &Project,
    timeline_id: TimelineId,
    track_index: usize,
    clip_id: ClipId,
) -> Option<(FrameIdx, FrameIdx)> {
    let clip = project.timelines[timeline_id]
        .tracks
        .get(track_index)?
        .clips
        .iter()
        .find(|c| c.id == clip_id)?;
    Some((clip.timeline_start, clip.timeline_end()))
}

/// Applica la modifica necessaria a *una* clip esistente che si
/// sovrappone a `[new_start, new_end)`: rimossa se completamente
/// coperta, accorciata da un bordo se sporge solo da un lato, divisa
/// in due se il nuovo intervallo cade nel suo mezzo (il pezzo
/// centrale, quello coperto, sparisce — comportamento "overwrite" di
/// un vero NLE). `old_start`/`old_end` sono lo stato
/// *attuale* della clip (letto dal chiamante prima di accodare
/// comandi, mai da uno stato immaginato). Ritorna `Some((id_sinistra,
/// id_destra))` solo nel caso di uno split, per permettere al
/// chiamante di ricollegare le due metà alla gemella coinvolta dalla
/// stessa operazione (vedi `make_room_for_ranges`). I comandi vengono
/// accodati a `commands`, non eseguiti subito.
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
) -> Option<(ClipId, ClipId)> {
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
        Some((clip_id, right_id))
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

/// Libera `[start, end)` di ciascuna `(track_index, start, end)` in
/// `ranges`, per far posto a nuove clip che stanno per essere inserite
/// lì (paste): le clip già presenti che si sovrappongono vengono
/// accorciate, divise o rimosse — mai lasciate sovrapposte con la
/// nuova clip sopra (bug segnalato: l'anteprima riproduceva la clip
/// sottostante invece di quella appena incollata).
///
/// Dividere una clip non tocca il `linked_group` della metà sinistra
/// (`SplitClip`, vedi doc): resta collegata a chiunque altro condivida
/// il gruppo, split o no. La metà *destra* invece è nuova e parte
/// scollegata — se altri membri del gruppo sono *anche loro* tra le
/// track coinvolte in `ranges` (il caso comune: si incolla sempre
/// l'intero gruppo video+audio insieme, vedi `copy_selected_clips`) e
/// vengono divisi dallo stesso taglio, le loro metà destre vengono
/// ricollegate tra loro subito dopo — stesso principio di
/// `split_at_playhead`. Un membro del gruppo fuori da `ranges` (si
/// sta incollando solo una parte del gruppo) resta semplicemente
/// intoccato, ancora nel gruppo originale.
///
/// I comandi vengono accodati a `commands`, non eseguiti subito: il
/// chiamante li unisce in un'unica `CompositeCommand` insieme
/// all'inserimento vero e proprio, per un solo passo di undo.
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

            let Some(_group) = group else { continue };
            let mut new_rights: Vec<(usize, ClipId)> = split_halves
                .map(|(_left, right)| (track_index, right))
                .into_iter()
                .collect();

            for (member_track, member_id) in group_members(project, timeline_id, track_index, clip_id) {
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
                if let Some((_, right)) = member_split {
                    new_rights.push((member_track, right));
                }
            }

            if new_rights.len() >= 2 {
                commands.push(Box::new(LinkClips::new(timeline_id, new_rights)));
            }
        }
    }
}


/// Taglia le sovrapposizioni rimaste su ogni track: dove due clip si
/// accavallano vince quella che comincia dopo (è quella appena arrivata
/// lì), e quella sotto viene accorciata fino al suo bordo, o rimossa se ne
/// resta coperta del tutto. Rete di sicurezza generale dopo le operazioni
/// che spostano clip in blocco (il ripple delete): una sovrapposizione
/// lasciata lì si vedrebbe e si *sentirebbe* — entrambe le tracce audio
/// insieme, il video di quella sotto.
///
/// I comandi vengono accodati a `commands`, non eseguiti subito.
pub fn cut_overlaps(
    project: &mut Project,
    timeline_id: TimelineId,
    commands: &mut Vec<Box<dyn Command>>,
) {
    for track_index in 0..project.timelines[timeline_id].tracks.len() {
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

/// Gli altri membri del gruppo collegato di una clip, se ne ha uno.
fn group_members(
    project: &Project,
    timeline_id: TimelineId,
    track_index: usize,
    clip_id: ClipId,
) -> Vec<(usize, ClipId)> {
    let Some(group) = project.timelines[timeline_id]
        .tracks
        .get(track_index)
        .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
        .and_then(|c| c.linked_group)
    else {
        return Vec::new();
    };
    project.timelines[timeline_id]
        .clips_in_group(group)
        .into_iter()
        .filter(|&(_, id)| id != clip_id)
        .collect()
}
