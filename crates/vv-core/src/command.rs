//! Command pattern per undo/redo. Ogni comando cattura da sé lo stato
//! necessario a invertirsi nel momento in cui viene applicato.

use crate::model::{
    Clip, ClipId, FrameIdx, Interpolation, Keyframed, LinkGroupId, Project, Rgba, TimelineId, Track,
    TrackKind, Transform,
};

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
        let gap_len = primary.timeline_len();

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

/// Quale bordo di una clip viene trimmato: `Start` cambia `source_in`
/// *e* `timeline_start` insieme (la fine sulla timeline resta ferma,
/// solo l'inizio si muove), `End` cambia solo `source_out` (l'inizio
/// resta fermo, solo la fine si muove).
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
    /// Nuovo `source_in` (bordo `Start`) o `source_out` (bordo `End`).
    pub new_value: FrameIdx,
    old: Option<(FrameIdx, FrameIdx, FrameIdx)>, // (source_in, source_out, timeline_start) precedenti
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
        self.old = Some((clip.source_in, clip.source_out, clip.timeline_start));
        match self.edge {
            TrimEdge::Start => {
                let delta = self.new_value - clip.source_in;
                clip.source_in = self.new_value;
                clip.timeline_start += delta;
            }
            TrimEdge::End => {
                clip.source_out = self.new_value;
            }
        }
        resort(track);
    }

    fn undo(&self, project: &mut Project) {
        let Some((source_in, source_out, timeline_start)) = self.old else {
            return;
        };
        let track = &mut project.timelines[self.timeline].tracks[self.track_index];
        if let Some(clip) = track.clips.iter_mut().find(|c| c.id == self.clip_id) {
            clip.source_in = source_in;
            clip.source_out = source_out;
            clip.timeline_start = timeline_start;
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
    /// Se impostato (`with_new_clip_id`), l'id della metà destra è questo
    /// invece di uno allocato al volo: serve a chi orchestra più split
    /// collegati (vedi `split_all_at_playhead`) per conoscere in anticipo
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
            original_source_out: None,
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

        let offset = self.split_at - clip.timeline_start;
        let split_source = clip.source_in + offset;

        self.original_source_out = Some(clip.source_out);
        let mut second_half = clip.clone();
        clip.source_out = split_source;
        // `clip` (la metà sinistra) mantiene il suo `linked_group`
        // invariato: è la stessa clip di prima, solo accorciata, e resta
        // collegata a chiunque altro condivida quel gruppo (che sia stato
        // diviso insieme o no). La metà destra è una clip nuova, che parte
        // scollegata — chi orchestra più split insieme (`split_all_at_playhead`)
        // ricollega le metà destre tra loro con un `LinkClips` a parte.
        second_half.id = new_id;
        second_half.source_in = split_source;
        second_half.timeline_start = self.split_at;
        second_half.linked_group = None;
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
    Transform(Transform),
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
            Some((KeyframeValue::Transform(v), i)) => {
                clip.effects.transform.upsert(self.frame, *v, *i);
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
                KeyframeValue::Transform(_) => {
                    clip.effects.transform.remove_at(self.frame);
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
    Transform,
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
            KeyframeValue::Transform(v) => clip.effects.transform.upsert(self.frame, v, interp),
            KeyframeValue::Gain(v) => clip.effects.gain_db.upsert(self.frame, v, interp),
            KeyframeValue::Color(v) => {
                if let Some(color) = &mut clip.effects.color {
                    color.upsert(self.frame, v, interp);
                }
            }
        }
    }
}
