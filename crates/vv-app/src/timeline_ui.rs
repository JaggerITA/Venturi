//! Widget timeline multi-traccia: clip, selezione (click, ctrl, shift,
//! rettangolo, sempre estesa ai gruppi collegati), drag, trim, playhead.
//! Le modifiche al progetto passano da `History::do_command`; la sola
//! selezione muta `TimelineState`. Disegnato col painter: per una griglia
//! densa di rettangoli costa meno dei widget annidati.

use std::collections::BTreeSet;

use vv_core::{
    Clip, ClipId, ClipSource, FadeEdge, FrameIdx, History, Keyframed, Project, TimelineId, Track,
    TrackFlag, TrackKind, TrimEdge,
};

const ROW_HEIGHT: f32 = 40.0;
const RULER_HEIGHT: f32 = 20.0;
const MIN_TIMELINE_SECS: f64 = 20.0;
const TRAILING_MARGIN_SECS: f64 = 5.0;
/// Altezza del separatore trascinabile fra il gruppo Video e il gruppo Audio.
const GROUP_DIVIDER_HEIGHT: f32 = 8.0;
/// Colonna fissa a sinistra della timeline (etichetta track + rimuovi),
/// non coinvolta nello scroll orizzontale — vedi `draw_track_headers`.
const TRACK_HEADER_WIDTH: f32 = 140.0;
const MIN_PANE_HEIGHT: f32 = 20.0;
/// Zona "nuova track" minima oltre l'ultima track quando il riquadro
/// scorre: senza, con molte track non ci sarebbe dove trascinarne una nuova.
const NEW_TRACK_ZONE_HEIGHT: f32 = 24.0;
const PANE_SCROLLBAR_WIDTH: f32 = 8.0;

/// (track, id): l'id è un contatore globale, la track serve a trovarla.
type ClipKey = (usize, ClipId);

/// Cosa mostra il pannello proprietà quando è selezionata una transizione
/// invece di una clip — alternativa a `TimelineState::selected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionSelection {
    /// Transizione su un solo bordo (`EffectStack::transition_in`/`_out`).
    Edge(ClipKey, FadeEdge),
    /// Transizione a cavallo, identificata dal suo `left_clip` (una clip ha
    /// al più una crossing sul proprio bordo destro) e dalla track.
    Crossing(usize, ClipId),
}

pub struct TimelineState {
    /// Clip selezionate. Vuoto se nessuna clip è selezionata (non deve
    /// essere confuso con "nessuna timeline": qui è solo lo stato della
    /// selezione dentro una timeline esistente).
    pub selected: BTreeSet<ClipKey>,
    /// Origine dello shift+click. Uno shift+click non la sposta, come nei
    /// file manager.
    selection_anchor: Option<ClipKey>,
    pub playhead: FrameIdx,
    pixels_per_sec: f32,
    /// `pixels_per_sec` dell'ultimo disegno: se è cambiato c'è stato uno zoom
    /// in questo frame, e lo scroll va corretto per ancorarlo alla testina.
    last_rendered_pps: f32,
    drag: Option<DragState>,
    /// Rettangolo di selezione in corso, in coordinate del contenuto (resta
    /// valido se lo scroll cambia).
    marquee: Option<MarqueeDrag>,
    /// Vuoto selezionato (track, inizio, fine), alternativo a `selected`:
    /// si chiude con ripple delete. Lo spazio in coda non è un vuoto.
    pub selected_gap: Option<(usize, FrameIdx, FrameIdx)>,
    /// Transizione selezionata (bordo singolo o crossing), alternativa a
    /// `selected`: un click su di lei sostituisce qualunque selezione di
    /// clip, anche multipla — il pannello proprietà mostra i suoi
    /// controlli al posto di quelli della clip.
    pub selected_transition: Option<TransitionSelection>,
    /// Clip copiate (Ctrl+C in `main.rs`), pronte per essere incollate
    /// (Ctrl+V) alla posizione del playhead. Vuoto se non è ancora mai
    /// stato copiato nulla in questa sessione.
    pub clipboard: Vec<ClipboardEntry>,
    trim: Option<TrimState>,
    fade_drag: Option<FadeDragState>,
    transition_drag: Option<TransitionDragState>,
    crossing_drag: Option<CrossingDragState>,
    /// Alt+drag sul corpo (non sulla maniglia) di un marker di transizione,
    /// in corso: duplica invece di ridimensionare. Non muta nulla da sé — il
    /// payload DnD (`vv_core::Transition`) parte già impostato da
    /// `begin_transition_duplicate_drag` e da lì lo tiene vivo
    /// `egui::DragAndDrop` finché dura il drag; questo campo serve solo a
    /// sapere quando il gesto finisce (vedi il ramo `drag_stopped`), per la
    /// clip di origine.
    transition_duplicate_drag: Option<ClipId>,
    volume_drag: Option<VolumeDragState>,
    /// Altezza del riquadro Video se l'utente ha trascinato il separatore
    /// (vedi `GROUP_DIVIDER_HEIGHT`); `None` = gruppi centrati di default.
    video_pane_height: Option<f32>,
    /// Scroll verticale del riquadro Video, misurato dal basso: le track
    /// video stanno appoggiate al separatore, come in un NLE.
    video_scroll: f32,
    audio_scroll: f32,
    /// Velocità residua (px/s) dello scroll cinetico da touchpad: a gesto
    /// finito la vista continua a scorrere e frena con attrito finché non
    /// arriva a `KINETIC_STOP_SPEED`, invece di fermarsi di scatto.
    hscroll_vel: f32,
    video_scroll_vel: f32,
    audio_scroll_vel: f32,
    /// In/out della timeline: porzione esportata.
    pub export_marks: crate::transport::MarkRange,
}

/// Una clip copiata. Id e gruppo si riassegnano all'incolla; la posizione
/// è relativa alla più a sinistra delle clip copiate.
#[derive(Clone)]
pub struct ClipboardEntry {
    pub track_index: usize,
    pub relative_start: FrameIdx,
    /// La clip com'era alla copia; id, posizione e gruppo si riassegnano
    /// all'incolla.
    pub clip: Clip,
    /// Fps della timeline di origine, in cui sono espressi `clip` e
    /// `relative_start`.
    pub timeline_fps: vv_core::Rational,
    /// Clip con lo stesso tag erano nello stesso gruppo alla copia.
    pub link_tag: Option<u64>,
}

struct MarqueeDrag {
    start: egui::Pos2,
    current: egui::Pos2,
}

struct DragState {
    /// La clip premuta: la sua posizione pilota la calamita, le altre la
    /// seguono con l'offset iniziale.
    clip_id: ClipId,
    /// Track di partenza: la track candidata (vedi `track_drag_target`)
    /// può differire durante il drag, i bound si ricalcolano ogni frame.
    track_index: usize,
    original_start: FrameIdx,
    accum_px: f32,
    /// (clip_id, track_index, offset) delle clip che seguono la primaria:
    /// la selezione all'inizio del drag, gruppi collegati compresi.
    followers: Vec<(ClipId, usize, FrameIdx)>,
    /// Drag iniziato con ALT: al rilascio si inseriscono delle copie, gli
    /// originali restano dove sono.
    duplicate: bool,
}

/// Trascinamento dell'handle di dissolvenza (fade-in o fade-out) di una
/// clip: nessun follower né neighbor, è sempre locale alla singola clip.
struct FadeDragState {
    clip_id: ClipId,
    track_index: usize,
    edge: FadeEdge,
    /// Valore originale (frame) di `fade_in`/`fade_out` prima del drag.
    original_value: FrameIdx,
    accum_px: f32,
}

/// Trascinamento dell'estremità di una transizione (durata): stessa forma
/// di `FadeDragState`, stessa unica clip coinvolta.
struct TransitionDragState {
    clip_id: ClipId,
    track_index: usize,
    edge: FadeEdge,
    /// Durata (frame) prima del drag.
    original_value: FrameIdx,
    accum_px: f32,
}

/// Trascinamento dell'estremità di una crossing transition: a differenza
/// di `TransitionDragState`, tocca sempre entrambi i lati alla pari (vedi
/// `CrossTransition::split`) — qui non serve un `FadeEdge`, solo sapere se
/// si sta afferrando l'estremità dentro la clip di sinistra o quella
/// dentro la clip di destra, per il segno dello spostamento.
struct CrossingDragState {
    track_index: usize,
    left_clip: ClipId,
    grabbed_left_side: bool,
    /// Durata totale (frame) prima del drag.
    original_duration: FrameIdx,
    /// Non può eccedere la durata delle due clip coinvolte: calcolato una
    /// volta all'inizio del drag, le clip non cambiano lunghezza nel
    /// frattempo.
    max_duration: FrameIdx,
    accum_px: f32,
}

/// Trascinamento verticale della riga del volume su una clip audio: come
/// `FadeDragState`, sempre locale alla singola clip, mai una selezione
/// multipla. A differenza di fade/trim, il gain si applica davvero (via
/// `PendingAction::SetGain`) a ogni frame di drag invece che solo al
/// rilascio — stessa catena di eventi dello slider del pannello proprietà,
/// così la waveform e il colore della clip seguono dal vivo. `group` tiene
/// insieme tutti quei commit in un solo passo di undo (vedi
/// `History::begin_group`).
struct VolumeDragState {
    clip_id: ClipId,
    track_index: usize,
    /// Gain (dB) prima del drag.
    original_db: f32,
    accum_px: f32,
    group: vv_core::GroupMark,
}

/// Trim di un bordo, separato dal drag vero e proprio.
struct TrimState {
    clip_id: ClipId,
    track_index: usize,
    edge: TrimEdge,
    /// Valore originale (frame, spazio timeline) della coordinata
    /// trimmata: `timeline_start` per `Start`, `timeline_end()` per `End`.
    original_value: FrameIdx,
    accum_px: f32,
    /// Range valido per il *nuovo* valore di `original_value`, già
    /// combinato con quello di tutti i `followers` (vedi
    /// `combined_trim_range`).
    min_value: FrameIdx,
    max_value: FrameIdx,
    /// (clip_id, track_index, offset, bordo) delle altre clip trimmate insieme;
    /// nel roll anche la vicina, col bordo opposto.
    followers: Vec<(ClipId, usize, FrameIdx, TrimEdge)>,
    /// Roll edit fra due clip adiacenti (solo per il cursore).
    roll: bool,
}

/// Cosa fa un drag partito vicino al bordo di una clip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EdgeZone {
    Trim(TrimEdge),
    /// Sul punto di contatto con la clip adiacente `neighbor`: `edge` è il
    /// bordo di questa clip, la vicina si muove con quello opposto.
    Roll { edge: TrimEdge, neighbor: ClipKey },
}

impl EdgeZone {
    fn edge(self) -> TrimEdge {
        match self {
            EdgeZone::Trim(edge) | EdgeZone::Roll { edge, .. } => edge,
        }
    }
}

/// Distanza (in pixel schermo) dal bordo di una clip entro cui un drag
/// parte come trim invece che come spostamento; ridotta per le clip molto
/// strette, altrimenti l'intera clip sarebbe "solo bordi".
const TRIM_HANDLE_PX: f32 = 8.0;
/// Semi-larghezza della zona di roll attorno al punto di contatto fra due
/// clip adiacenti; la zona di trim comincia subito dopo, verso l'interno.
const ROLL_HANDLE_PX: f32 = 4.0;
/// Raggio del pallino disegnato per l'handle di fade-in/fade-out.
const FADE_HANDLE_RADIUS: f32 = 4.0;
/// Semi-larghezza della zona cliccabile attorno all'handle: più larga del
/// pallino disegnato, per poterlo afferrare senza mirare al pixel.
const FADE_HANDLE_HIT_RADIUS: f32 = 9.0;
/// Banda in alto alla clip riservata agli handle di fade: sotto resta il
/// trim/roll del bordo, come negli altri NLE.
const FADE_HANDLE_ZONE_HEIGHT: f32 = 14.0;
/// Sotto questa larghezza la clip non mostra handle di fade: non ci
/// sarebbe spazio per afferrarli senza scontrarsi col trim.
const MIN_FADE_CLIP_WIDTH_PX: f32 = 20.0;
/// Banda in basso alla clip riservata al marker delle transizioni: speculare
/// alla banda di fade in alto, così le due non si contendono lo stesso hover.
const TRANSITION_HANDLE_ZONE_HEIGHT: f32 = 14.0;
/// Semi-larghezza della zona cliccabile attorno all'estremità (durata)
/// trascinabile di una transizione, come `FADE_HANDLE_HIT_RADIUS`.
const TRANSITION_HANDLE_HIT_RADIUS: f32 = 9.0;
/// Entro quanti pixel dal bordo di una clip un drop di transizione viene
/// accettato; oltre, il rilascio in mezzo alla clip non fa nulla.
const TRANSITION_DROP_ZONE_PX: f32 = 40.0;
/// Colore del marker di una transizione: anche l'evidenziazione del bordo
/// durante il drag, così il colore anticipa cosa comparirà al rilascio.
const TRANSITION_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 130, 235);
const TRANSITION_SELECTED_COLOR: egui::Color32 = egui::Color32::from_rgb(190, 197, 255);
/// Colore del marker di una crossing transition: tinta diversa da quella
/// di un bordo singolo, per segnalare a colpo d'occhio che questa "mangia"
/// anche la clip vicina invece di restare contro il trasparente.
const CROSSING_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 150, 90);
const CROSSING_SELECTED_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 195, 150);
/// Distanza verticale (px) entro cui il puntatore afferra la riga del
/// volume di una clip audio.
const VOLUME_LINE_HIT_PX: f32 = 5.0;

/// A 0,1 px/s un'ora sta in 360 px.
const MIN_PIXELS_PER_SEC: f32 = 0.1;
const MAX_PIXELS_PER_SEC: f32 = 800.0;

/// Sotto questa velocità lo scroll cinetico si ferma invece di strisciare
/// all'infinito. Attrito più basso di quello nativo di egui (1000 px/s²):
/// a quel valore lo swipe si sentiva a malapena, qui scivola più a lungo.
const KINETIC_STOP_SPEED: f32 = 15.0; // px/s
const KINETIC_FRICTION: f32 = 500.0; // px/s^2
/// Amplifica la velocità catturata dallo swipe: la sensibilità nativa
/// risultava fiacca, un gesto normale a malapena metteva in moto l'inerzia.
const KINETIC_VELOCITY_GAIN: f32 = 1.6;

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            selection_anchor: None,
            playhead: 0,
            pixels_per_sec: 60.0,
            // Stesso valore iniziale di `pixels_per_sec`: al primo frame non
            // c'è ancora nessuno zoom da compensare.
            last_rendered_pps: 60.0,
            drag: None,
            marquee: None,
            selected_gap: None,
            selected_transition: None,
            clipboard: Vec::new(),
            trim: None,
            fade_drag: None,
            transition_drag: None,
            crossing_drag: None,
            transition_duplicate_drag: None,
            volume_drag: None,
            video_pane_height: None,
            video_scroll: 0.0,
            audio_scroll: 0.0,
            hscroll_vel: 0.0,
            video_scroll_vel: 0.0,
            audio_scroll_vel: 0.0,
            export_marks: crate::transport::MarkRange::default(),
        }
    }
}

impl TimelineState {
    /// Imposta selezione e ancora da fuori (es. dopo un taglio).
    pub fn set_selection(&mut self, selected: BTreeSet<ClipKey>, anchor: Option<ClipKey>) {
        self.selected = selected;
        self.selection_anchor = anchor;
        self.selected_gap = None;
        self.selected_transition = None;
    }

    /// Una clip sola (o nessuna), per "selection follows playhead".
    pub fn set_single_selection(&mut self, clip: Option<ClipKey>) {
        self.set_selection(clip.into_iter().collect(), clip);
    }

    /// Toglie dalla selezione quel che sta su track bloccate.
    pub fn drop_locked(&mut self, timeline: &vv_core::Timeline) {
        self.selected.retain(|&(track_index, _)| !timeline.is_locked(track_index));
        if self
            .selection_anchor
            .is_some_and(|(track_index, _)| timeline.is_locked(track_index))
        {
            self.selection_anchor = None;
        }
        if self
            .selected_gap
            .is_some_and(|(track_index, _, _)| timeline.is_locked(track_index))
        {
            self.selected_gap = None;
        }
        if self.selected_transition.is_some_and(|sel| {
            let track_index = match sel {
                TransitionSelection::Edge((track_index, _), _) => track_index,
                TransitionSelection::Crossing(track_index, _) => track_index,
            };
            timeline.is_locked(track_index)
        }) {
            self.selected_transition = None;
        }
    }

    /// Svuota la selezione (clip, vuoto e transizione).
    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.selection_anchor = None;
        self.selected_gap = None;
        self.selected_transition = None;
    }

    /// Zoom orizzontale ancorato alla testina.
    pub fn zoom_in(&mut self) {
        self.set_pixels_per_sec(self.pixels_per_sec * ZOOM_STEP);
    }

    pub fn zoom_out(&mut self) {
        self.set_pixels_per_sec(self.pixels_per_sec / ZOOM_STEP);
    }

    fn set_pixels_per_sec(&mut self, value: f32) {
        self.pixels_per_sec = value.clamp(MIN_PIXELS_PER_SEC, MAX_PIXELS_PER_SEC);
    }
}

/// Fattore di zoom per passo di `zoom_in`/`zoom_out`.
const ZOOM_STEP: f32 = 1.25;

struct ClipVisual<'a> {
    track_index: usize,
    /// In prestito dal progetto; `Owned` solo nei test.
    clip: std::borrow::Cow<'a, Clip>,
    label: String,
    color: egui::Color32,
    /// La track è bloccata: la clip non si tocca.
    locked: bool,
    /// Esclusa dall'output: disattivata lei o la sua track video.
    muted: bool,
}

/// Comando raccolto durante il disegno (che tiene `project` in prestito) e
/// applicato dopo.
enum PendingAction {
    /// Sposta un gruppo trascinato. Le track nuove vanno create prima di
    /// risolvere gli `EffectiveTrack::New` di `moves`.
    Move {
        new_video_tracks: usize,
        new_audio_tracks: usize,
        moves: Vec<(ClipId, usize, EffectiveTrack, FrameIdx)>,
        duplicate: bool,
    },
    /// (clip_id, track, bordo, nuova posizione) per ogni clip, più i tratti
    /// che si prendono allungandosi: quel che c'era lì viene sovrascritto.
    Trim {
        trims: Vec<(ClipId, usize, TrimEdge, FrameIdx)>,
        overwritten: Vec<(usize, FrameIdx, FrameIdx)>,
    },
    /// Nuova durata (in frame) della dissolvenza in entrata o uscita.
    SetFade {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        new_value: FrameIdx,
    },
    /// Nuovo gain costante (dB), dal drag della riga volume in timeline.
    SetGain {
        track_index: usize,
        clip_id: ClipId,
        new_value: f32,
    },
    /// Un filtro del pannello Effects è stato rilasciato su questa clip:
    /// aggiunto in coda alla sua lista (o riattivato se già presente),
    /// attivo di default.
    ApplyFilter {
        track_index: usize,
        clip_id: ClipId,
        filter: vv_core::FilterKind,
    },
    /// Una transizione del pannello Effects è stata rilasciata vicino a un
    /// bordo di questa clip: sostituisce quella già presente su quel bordo
    /// (ridroppare accorcia/ripristina la durata di default), mai in coda a
    /// una lista come i filtri — un bordo ne ha al più una.
    ApplyTransition {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        kind: vv_core::TransitionKind,
    },
    /// Nuova durata (in frame) della transizione di un bordo, dal drag della
    /// sua estremità in timeline.
    SetTransitionDuration {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        new_value: FrameIdx,
    },
    /// Una transizione esistente è stata duplicata (Alt+drag dal suo
    /// corpo) e rilasciata vicino a un bordo: a differenza di
    /// `ApplyTransition`, non riparte dai default — mantiene gli stessi
    /// parametri dell'originale.
    DuplicateTransition {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        transition: vv_core::Transition,
    },
    /// Nuova durata (in frame) di una crossing transition, dal drag
    /// simmetrico della sua estremità — vedi `CrossingDragState`.
    SetCrossingDuration {
        track_index: usize,
        left_clip: ClipId,
        new_value: FrameIdx,
    },
    Unlink(usize, ClipId),
    /// Collega tutte le clip elencate (track_index, clip_id) in un unico
    /// gruppo nuovo — almeno 2.
    Link(Vec<ClipKey>),
    /// Rimuove la track a questo indice (e le sue clip).
    RemoveTrack(usize),
    SetTrackFlag(usize, TrackFlag, bool),
}

#[derive(Clone, Copy)]
struct TrackFlags {
    muted: bool,
    solo: bool,
    locked: bool,
}

/// Ordine di disegno: Video dall'indice più alto (la nuova sta in cima),
/// poi Audio in ordine. Indipendente dall'ordine in `tracks`.
fn track_row_order(track_kinds: &[TrackKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..track_kinds.len())
        .filter(|&i| track_kinds[i] == TrackKind::Video)
        .collect();
    order.reverse();
    order.extend((0..track_kinds.len()).filter(|&i| track_kinds[i] == TrackKind::Audio));
    order
}

/// Geometria verticale (`y` locali al contenuto) dei due riquadri, Video
/// sopra e Audio sotto al separatore, ognuno con il proprio scroll.
#[derive(Clone, Copy, Debug)]
struct PaneLayout {
    video_pane: egui::Rangef,
    audio_pane: egui::Rangef,
    video_rows_top: f32,
    audio_rows_top: f32,
    video_count: usize,
    audio_count: usize,
    divider_height: f32,
    video_max_scroll: f32,
    audio_max_scroll: f32,
    /// Limiti dell'altezza del riquadro Video trascinando il separatore.
    video_height_range: egui::Rangef,
}

impl PaneLayout {
    /// Clampa anche gli scroll in `state` ai limiti correnti.
    fn new(
        avail_below_ruler: f32,
        video_count: usize,
        audio_count: usize,
        state: &mut TimelineState,
    ) -> Self {
        let divider_height = if video_count > 0 && audio_count > 0 {
            GROUP_DIVIDER_HEIGHT
        } else {
            0.0
        };
        let rows_avail = (avail_below_ruler - divider_height).max(0.0);
        let video_rows = video_count as f32 * ROW_HEIGHT;
        let audio_rows = audio_count as f32 * ROW_HEIGHT;
        let default_video_height = if video_rows + audio_rows <= rows_avail {
            (rows_avail - video_rows - audio_rows) / 2.0 + video_rows
        } else {
            rows_avail * video_count as f32 / (video_count + audio_count) as f32
        };
        let min_height = if divider_height > 0.0 {
            MIN_PANE_HEIGHT.min(rows_avail / 2.0)
        } else {
            0.0
        };
        let video_height_range = egui::Rangef::new(min_height, rows_avail - min_height);
        let video_height = if divider_height > 0.0 {
            state
                .video_pane_height
                .unwrap_or(default_video_height)
                .clamp(video_height_range.min, video_height_range.max)
        } else {
            default_video_height
        };
        let audio_height = rows_avail - video_height;

        let max_scroll = |rows: f32, pane: f32| {
            if rows > pane {
                rows + NEW_TRACK_ZONE_HEIGHT - pane
            } else {
                0.0
            }
        };
        let video_max_scroll = max_scroll(video_rows, video_height);
        let audio_max_scroll = max_scroll(audio_rows, audio_height);
        state.video_scroll = state.video_scroll.clamp(0.0, video_max_scroll);
        state.audio_scroll = state.audio_scroll.clamp(0.0, audio_max_scroll);

        let video_bottom = RULER_HEIGHT + video_height;
        Self {
            video_pane: egui::Rangef::new(RULER_HEIGHT, video_bottom),
            audio_pane: egui::Rangef::new(
                video_bottom + divider_height,
                RULER_HEIGHT + avail_below_ruler.max(divider_height),
            ),
            video_rows_top: video_bottom - video_rows + state.video_scroll,
            audio_rows_top: video_bottom + divider_height - state.audio_scroll,
            video_count,
            audio_count,
            divider_height,
            video_max_scroll,
            audio_max_scroll,
            video_height_range,
        }
    }

    fn video_height(&self) -> f32 {
        self.video_pane.span()
    }

    fn video_rows_bottom(&self) -> f32 {
        self.video_rows_top + self.video_count as f32 * ROW_HEIGHT
    }

    fn audio_rows_bottom(&self) -> f32 {
        self.audio_rows_top + self.audio_count as f32 * ROW_HEIGHT
    }

    fn pane(&self, kind: TrackKind) -> egui::Rangef {
        match kind {
            TrackKind::Video => self.video_pane,
            TrackKind::Audio => self.audio_pane,
        }
    }

    /// `y` di una riga di `track_row_order`.
    fn row_y(&self, row: usize) -> f32 {
        if row < self.video_count {
            self.video_rows_top + row as f32 * ROW_HEIGHT
        } else {
            self.audio_rows_top + (row - self.video_count) as f32 * ROW_HEIGHT
        }
    }

    /// Riga (di `track_row_order`) più vicina a `y`, dentro al riquadro
    /// che contiene `y`.
    fn row_at_y(&self, y: f32) -> usize {
        let in_video = self.video_count > 0 && (self.audio_count == 0 || y < self.audio_pane.min);
        if in_video {
            let row = ((y - self.video_rows_top) / ROW_HEIGHT).floor().max(0.0) as usize;
            row.min(self.video_count - 1)
        } else {
            let row = ((y - self.audio_rows_top) / ROW_HEIGHT).floor().max(0.0) as usize;
            self.video_count + row.min(self.audio_count.saturating_sub(1))
        }
    }

    /// `y` sopra una track visibile (non in una zona vuota né nascosta
    /// dallo scroll).
    fn is_over_rows(&self, y: f32) -> bool {
        (self.video_pane.contains(y) && y >= self.video_rows_top && y < self.video_rows_bottom())
            || (self.audio_pane.contains(y)
                && y >= self.audio_rows_top
                && y < self.audio_rows_bottom())
    }
}

enum TrackDragTarget {
    Track(usize),
    NewTrack,
}

/// Track candidata di un drag: una esistente, o `New(depth)` da creare al
/// rilascio (`depth` 1-based: un follower può aver bisogno di più track
/// nuove per tenere la spaziatura del gruppo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveTrack {
    Existing(usize),
    New(usize),
}

/// Dove atterrerebbe una clip `kind` trascinata a `local_y`: una track o
/// una nuova (zona sopra le Video / sotto le Audio). `None` nel riquadro
/// dell'altro tipo o sul separatore.
fn track_drag_target(
    local_y: f32,
    kind: TrackKind,
    row_order: &[usize],
    layout: &PaneLayout,
) -> Option<TrackDragTarget> {
    match kind {
        TrackKind::Video => {
            if local_y >= layout.video_pane.max {
                return None;
            }
            let y = local_y.max(layout.video_pane.min);
            if y < layout.video_rows_top {
                return Some(TrackDragTarget::NewTrack);
            }
            if y < layout.video_rows_bottom() {
                return row_order.get(layout.row_at_y(y)).copied().map(TrackDragTarget::Track);
            }
            None
        }
        TrackKind::Audio => {
            if local_y < layout.audio_pane.min {
                return None;
            }
            let y = local_y.min(layout.audio_pane.max - 1.0);
            if y >= layout.audio_rows_bottom() {
                return Some(TrackDragTarget::NewTrack);
            }
            if y >= layout.audio_rows_top {
                return row_order.get(layout.row_at_y(y)).copied().map(TrackDragTarget::Track);
            }
            None
        }
    }
}

/// Drop su una track esistente: se si trascina del video su una track
/// video si atterra lì, altrimenti sulla track di sempre. `None` se è
/// bloccata.
fn media_pool_drop_target(
    track: usize,
    has_video: bool,
    track_kind: TrackKind,
    locked: bool,
) -> Option<MediaDropTarget> {
    if locked {
        None
    } else if has_video && track_kind == TrackKind::Video {
        Some(MediaDropTarget::Track(track))
    } else {
        Some(MediaDropTarget::Default)
    }
}

/// Track target di ogni clip di un gruppo trascinato: i follower si
/// spostano dello stesso numero di righe della primaria (al contrario se
/// di tipo diverso: video e audio crescono in versi opposti). Oltre
/// l'ultima track del proprio tipo diventano `New(depth)`, nel verso
/// opposto si fermano alla più vicina.
fn drag_group_row_targets(
    primary_id: ClipId,
    primary_track: usize,
    primary_target: EffectiveTrack,
    followers: &[(ClipId, usize, FrameIdx)],
    track_kinds: &[TrackKind],
    row_of_track: &[usize],
    row_order: &[usize],
    video_count: usize,
    track_count: usize,
) -> Vec<(ClipId, EffectiveTrack)> {
    let primary_kind = track_kinds[primary_track];
    let primary_original_row = row_of_track[primary_track] as isize;
    let primary_target_row = match primary_target {
        EffectiveTrack::Existing(track) => row_of_track[track] as isize,
        EffectiveTrack::New(_) => match primary_kind {
            TrackKind::Video => -1,
            TrackKind::Audio => track_count as isize,
        },
    };
    let delta_row = primary_target_row - primary_original_row;

    let mut targets = vec![(primary_id, primary_target)];
    for &(follower_id, follower_track, _) in followers {
        let follower_kind = track_kinds[follower_track];
        let signed_delta = if follower_kind == primary_kind {
            delta_row
        } else {
            -delta_row
        };
        let candidate_row = row_of_track[follower_track] as isize + signed_delta;
        let target = match follower_kind {
            TrackKind::Video if candidate_row < 0 => EffectiveTrack::New((-candidate_row) as usize),
            TrackKind::Audio if candidate_row > track_count as isize - 1 => {
                EffectiveTrack::New((candidate_row - (track_count as isize - 1)) as usize)
            }
            TrackKind::Video => {
                EffectiveTrack::Existing(row_order[candidate_row.min(video_count as isize - 1) as usize])
            }
            TrackKind::Audio => EffectiveTrack::Existing(
                row_order[candidate_row.max(video_count as isize) as usize],
            ),
        };
        targets.push((follower_id, target));
    }
    targets
}

/// Colonna fissa delle intestazioni. Disegnata a mano: un `add_space`
/// legato all'altezza farebbe crescere il pannello all'infinito.
fn draw_track_headers(
    ui: &mut egui::Ui,
    track_kinds: &[TrackKind],
    track_labels: &[String],
    track_flags: &[TrackFlags],
    row_order: &[usize],
    row_y: &[f32],
    layout: &PaneLayout,
    video_pane_height: &mut Option<f32>,
    pending: &mut Option<PendingAction>,
    playhead: FrameIdx,
    fps: f64,
) {
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(TRACK_HEADER_WIDTH, RULER_HEIGHT), egui::Sense::hover());
    let origin = rect.min;
    let full_clip = ui.clip_rect();
    let text_color = ui.visuals().text_color();

    // Timestamp della posizione testina in formato HH:MM:SS:FF, nella riga
    // del righello (come in DaVinci Resolve).
    let playhead_secs = playhead as f64 / fps;
    ui.painter().text(
        egui::pos2(origin.x + TRACK_HEADER_WIDTH / 2.0, origin.y + RULER_HEIGHT / 2.0),
        egui::Align2::CENTER_CENTER,
        format_timecode(playhead_secs, fps),
        egui::FontId::monospace(14.0),
        egui::Color32::WHITE,
    );

    for &track_index in row_order {
        let kind = track_kinds[track_index];
        let pane = layout.pane(kind);
        ui.set_clip_rect(full_clip.intersect(egui::Rect::from_x_y_ranges(
            rect.x_range(),
            (origin.y + pane.min)..=(origin.y + pane.max),
        )));
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + row_y[track_index]),
            egui::vec2(TRACK_HEADER_WIDTH, ROW_HEIGHT),
        );
        ui.painter().text(
            row_rect.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &track_labels[track_index],
            egui::FontId::proportional(14.0),
            text_color,
        );

        let flags = track_flags[track_index];
        let toggle = |x: f32, id: &str, hover: &str, paint: &dyn Fn(&egui::Painter, egui::Rect)| {
            let rect = egui::Rect::from_center_size(
                egui::pos2(row_rect.left() + x, row_rect.center().y),
                egui::vec2(20.0, 20.0),
            );
            let resp = ui
                .interact(rect, ui.id().with(id).with(track_index), egui::Sense::click())
                .on_hover_text(hover);
            if resp.hovered() {
                ui.painter().rect_filled(rect, 3.0, egui::Color32::from_gray(60));
            }
            paint(ui.painter(), rect);
            resp.clicked()
        };
        if toggle(42.0, "lock_track", &t!("timeline.lock_track"), &|p, r| {
            paint_lock_icon(p, r, flags.locked)
        }) {
            *pending = Some(PendingAction::SetTrackFlag(
                track_index,
                TrackFlag::Locked,
                !flags.locked,
            ));
        }
        match kind {
            TrackKind::Video => {
                if toggle(66.0, "mute_track", &t!("timeline.disable_video_track"), &|p, r| {
                    paint_film_icon(p, r, !flags.muted)
                }) {
                    *pending = Some(PendingAction::SetTrackFlag(
                        track_index,
                        TrackFlag::Muted,
                        !flags.muted,
                    ));
                }
            }
            TrackKind::Audio => {
                let solo_color = egui::Color32::from_rgb(215, 170, 40);
                if toggle(66.0, "solo_track", &t!("timeline.solo"), &|p, r| {
                    paint_letter_button(p, r, "S", flags.solo.then_some(solo_color))
                }) {
                    *pending =
                        Some(PendingAction::SetTrackFlag(track_index, TrackFlag::Solo, !flags.solo));
                }
                let mute_color = egui::Color32::from_rgb(200, 60, 60);
                if toggle(90.0, "mute_track", &t!("timeline.mute"), &|p, r| {
                    paint_letter_button(p, r, "M", flags.muted.then_some(mute_color))
                }) {
                    *pending = Some(PendingAction::SetTrackFlag(
                        track_index,
                        TrackFlag::Muted,
                        !flags.muted,
                    ));
                }
            }
        }

        let is_last_of_kind = track_kinds.iter().filter(|k| **k == kind).count() <= 1;
        const REMOVE_BTN_SIZE: f32 = 18.0;
        let remove_rect = egui::Rect::from_center_size(
            egui::pos2(row_rect.right() - 14.0, row_rect.center().y),
            egui::vec2(REMOVE_BTN_SIZE, REMOVE_BTN_SIZE),
        );
        let sense = if is_last_of_kind {
            egui::Sense::hover()
        } else {
            egui::Sense::click()
        };
        let remove_resp = ui
            .interact(remove_rect, ui.id().with("remove_track").with(track_index), sense)
            .on_hover_text(if is_last_of_kind {
                t!("timeline.cannot_remove_last_track")
            } else {
                t!("timeline.remove_track")
            });
        if !is_last_of_kind && remove_resp.hovered() {
            ui.painter()
                .rect_filled(remove_rect, 3.0, egui::Color32::from_gray(70));
        }
        ui.painter().text(
            remove_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(14.0),
            if is_last_of_kind {
                egui::Color32::from_gray(90)
            } else {
                text_color
            },
        );
        if remove_resp.clicked() {
            *pending = Some(PendingAction::RemoveTrack(track_index));
        }
    }

    ui.set_clip_rect(full_clip);

    if layout.divider_height > 0.0 {
        let divider_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + layout.video_pane.max),
            egui::vec2(TRACK_HEADER_WIDTH, layout.divider_height),
        );
        interact_divider(
            ui,
            ui.painter(),
            divider_rect,
            ui.id().with("timeline_header_track_split"),
            layout,
            video_pane_height,
        );
    }
}

/// Separatore trascinabile Video/Audio: presente sia nella colonna degli
/// header sia nell'area scrollabile, con lo stato condiviso.
fn interact_divider(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    id: egui::Id,
    layout: &PaneLayout,
    video_pane_height: &mut Option<f32>,
) {
    let resp = ui.interact(rect, id, egui::Sense::drag());
    let active = resp.hovered() || resp.dragged();
    if active {
        ui.ctx()
            .output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
    }
    if resp.dragged() {
        let range = layout.video_height_range;
        *video_pane_height =
            Some((layout.video_height() + resp.drag_delta().y).clamp(range.min, range.max));
    }
    painter.hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, egui::Color32::from_gray(if active { 160 } else { 80 })),
    );
}

/// Scrollbar verticale di un riquadro; `offset` misurato dall'alto.
/// Restituisce il nuovo offset se l'utente la trascina.
fn pane_scrollbar(
    ui: &egui::Ui,
    painter: &egui::Painter,
    track_rect: egui::Rect,
    id: egui::Id,
    offset: f32,
    max_offset: f32,
) -> Option<f32> {
    if max_offset <= 0.0 || track_rect.height() <= 0.0 {
        return None;
    }
    let content_height = track_rect.height() + max_offset;
    let thumb_height = (track_rect.height() * track_rect.height() / content_height)
        .max(16.0)
        .min(track_rect.height());
    let travel = track_rect.height() - thumb_height;
    let thumb_top = track_rect.top() + travel * offset / max_offset;
    let thumb_rect = egui::Rect::from_min_size(
        egui::pos2(track_rect.left(), thumb_top),
        egui::vec2(track_rect.width(), thumb_height),
    );
    let resp = ui.interact(track_rect, id, egui::Sense::click_and_drag());
    let active = resp.hovered() || resp.dragged();
    painter.rect_filled(track_rect, 4.0, egui::Color32::from_black_alpha(90));
    painter.rect_filled(
        thumb_rect.shrink2(egui::vec2(1.0, 1.0)),
        4.0,
        egui::Color32::from_gray(if active { 170 } else { 120 }),
    );
    if resp.dragged() && travel > 0.0 {
        return Some((offset + resp.drag_delta().y * max_offset / travel).clamp(0.0, max_offset));
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && travel > 0.0
    {
        let target = (pos.y - track_rect.top() - thumb_height / 2.0) / travel * max_offset;
        return Some(target.clamp(0.0, max_offset));
    }
    None
}

fn paint_lock_icon(painter: &egui::Painter, rect: egui::Rect, locked: bool) {
    let color = if locked {
        egui::Color32::from_gray(235)
    } else {
        egui::Color32::from_gray(110)
    };
    let c = rect.center();
    let body = egui::Rect::from_min_max(c + egui::vec2(-5.0, -1.0), c + egui::vec2(5.0, 6.0));
    painter.rect_filled(body, 1.5, color);
    // Da aperto, la gamba destra dell'arco non arriva al corpo.
    let right_leg_end = if locked { -1.0 } else { -4.0 };
    let mut points = vec![c + egui::vec2(-3.5, -1.0), c + egui::vec2(-3.5, -3.5)];
    points.extend((0..=8).map(|i| {
        let a = std::f32::consts::PI * (1.0 + i as f32 / 8.0);
        c + egui::vec2(3.5 * a.cos(), -3.5 + 3.5 * a.sin())
    }));
    points.push(c + egui::vec2(3.5, right_leg_end));
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.6, color)));
}

/// Pellicola; barrata in rosso se la track è disattivata.
fn paint_film_icon(painter: &egui::Painter, rect: egui::Rect, enabled: bool) {
    let color = egui::Color32::from_gray(if enabled { 200 } else { 100 });
    let film = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 11.0));
    painter.rect_stroke(film, 1.0, egui::Stroke::new(1.3, color), egui::StrokeKind::Inside);
    for i in 0..4 {
        let x = film.left() + 2.5 + i as f32 * 3.0;
        for y in [film.top() + 2.0, film.bottom() - 2.0] {
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(1.4, 1.4)),
                0.0,
                color,
            );
        }
    }
    if !enabled {
        painter.line_segment(
            [film.left_bottom() + egui::vec2(-1.0, 1.0), film.right_top() + egui::vec2(1.0, -1.0)],
            egui::Stroke::new(1.6, egui::Color32::from_rgb(220, 70, 70)),
        );
    }
}

/// Ingranaggio a mano (anello + denti radiali): niente glifo Unicode, che
/// su alcune piattaforme (Asahi) manca nei font di egui (vedi il commento
/// sul link icon in `paint_clip_overlay`).
pub(crate) fn paint_gear_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.6, color);
    painter.circle_stroke(center, radius * 0.55, stroke);
    painter.circle_filled(center, radius * 0.16, color);
    const TEETH: usize = 8;
    for i in 0..TEETH {
        let angle = std::f32::consts::TAU * i as f32 / TEETH as f32;
        let dir = egui::vec2(angle.cos(), angle.sin());
        painter.line_segment([center + dir * radius * 0.55, center + dir * radius], stroke);
    }
}

/// Pulsante "S"/"M": pieno di `active` quando è attivo.
fn paint_letter_button(
    painter: &egui::Painter,
    rect: egui::Rect,
    letter: &str,
    active: Option<egui::Color32>,
) {
    let button = rect.shrink(2.0);
    let text_color = match active {
        Some(fill) => {
            painter.rect_filled(button, 3.0, fill);
            egui::Color32::BLACK
        }
        None => {
            painter.rect_stroke(
                button,
                3.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                egui::StrokeKind::Inside,
            );
            egui::Color32::from_gray(150)
        }
    };
    painter.text(
        button.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(11.0),
        text_color,
    );
}

/// Intervallo tra tacche maggiori dalla sequenza 1-2-5, il primo che le
/// tiene ad almeno `MIN_MAJOR_TICK_PX`.
fn nice_tick_interval_secs(pixels_per_sec: f32) -> f64 {
    const MIN_MAJOR_TICK_PX: f32 = 70.0;
    const CANDIDATES: &[f64] = &[
        1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
        14400.0,
    ];
    CANDIDATES
        .iter()
        .copied()
        .find(|&c| c as f32 * pixels_per_sec >= MIN_MAJOR_TICK_PX)
        .unwrap_or(*CANDIDATES.last().unwrap())
}

/// Timecode HH:MM:SS:FF non drop-frame: con fps non interi (29,97) i
/// secondi si contano sull'fps nominale arrotondato, come negli NLE.
fn format_timecode(total_secs: f64, fps: f64) -> String {
    let nominal = (fps.round() as i64).max(1);
    let frame = (total_secs.max(0.0) * fps).round() as i64;
    let (h, m) = (frame / (nominal * 3600), frame / (nominal * 60) % 60);
    let (s, f) = (frame / nominal % 60, frame % nominal);
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

/// Tacche su tre livelli: maggiori con timecode, medie ogni N frame,
/// un frame ciascuna; un livello più fitto di `MIN_TICK_SPACING_PX` non si
/// disegna.
fn draw_ruler_ticks(
    painter: &egui::Painter,
    origin: egui::Pos2,
    visible_x: egui::Rect,
    pixels_per_sec: f32,
    fps: f64,
) {
    if !visible_x.is_positive() {
        return; // righello completamente fuori dal viewport scrollato
    }

    const MIN_TICK_SPACING_PX: f32 = 8.0;
    let tick_color = egui::Color32::from_gray(110);
    let label_color = egui::Color32::from_gray(200);
    let minor_color = egui::Color32::from_gray(70);
    let medium_color = egui::Color32::from_gray(90);

    // Altezze dei tre livelli di tacche (dal basso verso l'alto).
    const FRAME_TICK_HEIGHT: f32 = 5.0;
    const MEDIUM_TICK_HEIGHT: f32 = 10.0;
    const MAJOR_TICK_HEIGHT: f32 = RULER_HEIGHT;

    // Range in secondi effettivamente visibile, non l'intera durata della
    // timeline (migliaia di tacche fuori schermo altrimenti).
    let visible_start_secs = ((visible_x.min.x - origin.x) / pixels_per_sec.max(1e-6)) as f64;
    let visible_end_secs = ((visible_x.max.x - origin.x) / pixels_per_sec.max(1e-6)) as f64;

    // Livello 2: tacche maggiori con etichetta HH:MM:SS:FF, intervallo
    // adattivo "pulito" (sequenza 1-2-5) — sempre visibile.
    let major_secs = nice_tick_interval_secs(pixels_per_sec);
    let first_major = (visible_start_secs / major_secs).floor() as i64;
    let last_major = (visible_end_secs / major_secs).ceil() as i64;
    for i in first_major..=last_major {
        let secs = i as f64 * major_secs;
        if secs < 0.0 {
            continue;
        }
        let x = origin.x + (secs * pixels_per_sec as f64) as f32;
        painter.line_segment(
            [egui::pos2(x, origin.y), egui::pos2(x, origin.y + MAJOR_TICK_HEIGHT)],
            egui::Stroke::new(1.0, tick_color),
        );
        painter.text(
            egui::pos2(x + 3.0, origin.y + 2.0),
            egui::Align2::LEFT_TOP,
            format_timecode(secs, fps),
            egui::FontId::proportional(10.0),
            label_color,
        );
    }

    // Tacche medie, solo se più fitte delle maggiori.
    let px_per_frame = pixels_per_sec / fps.max(1e-9) as f32;

    // Calcola l'intervallo in frame per le tacche medie: il più piccolo
    // multiplo "pulito" (1, 2, 5, 10, 25, 50...) che tiene le tacche ad
    // almeno MIN_TICK_SPACING_PX di distanza.
    let medium_interval_frames = {
        const MEDIUM_CANDIDATES: &[i64] = &[1, 2, 5, 10, 25, 50, 100, 250, 500];
        MEDIUM_CANDIDATES
            .iter()
            .copied()
            .find(|&c| c as f32 * px_per_frame >= MIN_TICK_SPACING_PX)
            .unwrap_or(*MEDIUM_CANDIDATES.last().unwrap())
    };

    // Le tacche medie sono utili solo se sono più vicine delle maggiori e
    // non si sovrappongono esattamente a loro (altrimenti sarebbero ridondanti).
    let major_interval_frames = (major_secs * fps) as i64;
    if medium_interval_frames < major_interval_frames {
        let first_frame = (visible_start_secs * fps).floor().max(0.0) as i64;
        let last_frame = (visible_end_secs * fps).ceil().max(0.0) as i64;
        for frame in (first_frame..=last_frame).step_by(medium_interval_frames as usize) {
            // Salta le posizioni dove c'è già una tacca maggiore (ridondante).
            let secs = frame as f64 / fps;
            let major_at_this_pos = ((secs / major_secs).round() * major_secs - secs).abs() < 1e-9;
            if major_at_this_pos {
                continue;
            }
            let x = origin.x + frame as f32 * px_per_frame;
            painter.line_segment(
                [
                    egui::pos2(x, origin.y + RULER_HEIGHT - MEDIUM_TICK_HEIGHT),
                    egui::pos2(x, origin.y + RULER_HEIGHT),
                ],
                egui::Stroke::new(1.0, medium_color),
            );
        }
    }

    // Livello 0: tacche per ogni singolo frame — le più corte, visibili solo
    // quando lo zoom è alto abbastanza da non farle toccare.
    if px_per_frame >= MIN_TICK_SPACING_PX {
        let first_frame = (visible_start_secs * fps).floor().max(0.0) as i64;
        let last_frame = (visible_end_secs * fps).ceil().max(0.0) as i64;
        for frame in first_frame..=last_frame {
            // Salta le posizioni dove c'è già una tacca media o maggiore.
            let is_medium_pos = frame % medium_interval_frames == 0;
            let secs = frame as f64 / fps;
            let is_major_pos = ((secs / major_secs).round() * major_secs - secs).abs() < 1e-9;
            if is_medium_pos || is_major_pos {
                continue;
            }
            let x = origin.x + frame as f32 * px_per_frame;
            painter.line_segment(
                [
                    egui::pos2(x, origin.y + RULER_HEIGHT - FRAME_TICK_HEIGHT),
                    egui::pos2(x, origin.y + RULER_HEIGHT),
                ],
                egui::Stroke::new(1.0, minor_color),
            );
        }
    }
}

/// Payload del drag&drop di un media verso la timeline: dal media pool
/// (tutto il media) o dal viewer (la porzione tra i marker in/out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDrag {
    pub media_id: vv_core::MediaId,
    pub source_in: FrameIdx,
    pub source_out: FrameIdx,
    pub streams: DragStreams,
}

/// Quali stream del media finiscono in timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragStreams {
    #[default]
    All,
    VideoOnly,
    AudioOnly,
}

impl MediaDrag {
    /// Durata iniziale di un'immagine dal pool (la sua `duration_frames` è un
    /// sentinel), come i generatori.
    const DEFAULT_IMAGE_SECS: f64 = 5.0;

    pub fn whole(media_id: vv_core::MediaId, meta: &vv_core::MediaMeta) -> Self {
        let source_out = if meta.is_image() {
            (meta.fps.as_f64() * Self::DEFAULT_IMAGE_SECS).round() as FrameIdx
        } else {
            meta.duration_frames
        };
        Self {
            media_id,
            source_in: 0,
            source_out,
            streams: DragStreams::All,
        }
    }

    pub fn takes_video(&self, meta: &vv_core::MediaMeta) -> bool {
        meta.has_video && self.streams != DragStreams::AudioOnly
    }

    pub fn takes_audio(&self, meta: &vv_core::MediaMeta) -> bool {
        meta.has_audio && self.streams != DragStreams::VideoOnly
    }

    /// Durata in frame *sorgente* (fps del media): i marker in/out
    /// dell'anteprima vivono in quello spazio.
    pub fn source_len(&self) -> FrameIdx {
        self.source_out - self.source_in
    }

    /// Quanto occuperà sulla timeline, conformato a `rate` (vedi
    /// `Clip::rate`): quel che conta per il ghost del drop e per la
    /// calamita, che lavorano in frame di timeline.
    pub fn timeline_len(&self, rate: vv_core::Rational) -> FrameIdx {
        rate.scale_round(self.source_out) - rate.scale_round(self.source_in)
    }
}

/// Payload effettivo del drag&drop: più media selezionati insieme nel
/// media pool vengono accodati sulla timeline nell'ordine in cui compaiono
/// lì, quindi il payload è una lista ordinata, non un singolo media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDragSet {
    pub items: Vec<MediaDrag>,
}

impl MediaDragSet {
    pub fn one(drag: MediaDrag) -> Self {
        Self { items: vec![drag] }
    }
}

/// Effetti del pannello Effects: generano una clip senza media sorgente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    SolidColor,
    Text,
}

impl Generator {
    pub const ALL: [Generator; 2] = [Generator::SolidColor, Generator::Text];

    const DEFAULT_SECS: f64 = 5.0;

    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Generator::SolidColor => t!("generator.solid_color"),
            Generator::Text => t!("generator.text"),
        }
    }

    pub fn default_len(self, timeline_fps: vv_core::Rational) -> FrameIdx {
        (timeline_fps.as_f64() * Self::DEFAULT_SECS).round() as FrameIdx
    }
}

/// Cosa si sta trascinando verso la timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineDrag {
    Media(MediaDragSet),
    Generator(Generator),
}

impl TimelineDrag {
    pub fn hovered(resp: &egui::Response) -> Option<Self> {
        resp.dnd_hover_payload::<MediaDragSet>()
            .map(|set| Self::Media((*set).clone()))
            .or_else(|| resp.dnd_hover_payload::<Generator>().map(|g| Self::Generator(*g)))
    }

    pub fn released(resp: &egui::Response) -> Option<Self> {
        // `take_payload` scarta il payload anche se il tipo non combacia:
        // va scelto il tipo giusto prima di prenderlo. Un `FilterKind`, una
        // `TransitionKind` o una `Transition` intera (duplicazione via
        // Alt+drag) non sono mai un `TimelineDrag` (si rilasciano solo su
        // una clip, gestito nel loop delle clip): se è uno di quelli in
        // corso, uscire subito, altrimenti il ramo `MediaDragSet` sotto lo
        // prenderebbe e distruggerebbe senza riuscire a interpretarlo, e il
        // rilascio sulla clip non vedrebbe più nulla (vedi la stessa svista
        // già commessa e riparata per `FilterKind`).
        if egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(&resp.ctx)
            || egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(&resp.ctx)
            || egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(&resp.ctx)
        {
            return None;
        }
        if egui::DragAndDrop::has_payload_of_type::<Generator>(&resp.ctx) {
            resp.dnd_release_payload::<Generator>().map(|g| Self::Generator(*g))
        } else {
            resp.dnd_release_payload::<MediaDragSet>()
                .map(|set| Self::Media((*set).clone()))
        }
    }

    fn is_media(&self) -> bool {
        matches!(self, Self::Media(_))
    }
}

/// Filtri del pannello Effects, in ordine di comparsa lì: a differenza dei
/// `Generator`, si applicano a una clip video esistente invece di
/// generarne una nuova, e per questo restano fuori da `TimelineDrag`
/// (niente ghost sulle zone vuote, niente nuove track). Il tipo condiviso
/// con `EffectStack::filters` (`vv_core::FilterKind`) resta l'unica fonte
/// di verità su "quali filtri esistono": qui solo la loro etichetta.
pub const ALL_FILTER_KINDS: [vv_core::FilterKind; 1] = [vv_core::FilterKind::Grayscale];

pub fn filter_label(kind: vv_core::FilterKind) -> std::borrow::Cow<'static, str> {
    match kind {
        vv_core::FilterKind::Grayscale => t!("filter.grayscale"),
    }
}

/// Quanto occupa sulla timeline il media trascinato: la sua durata in
/// frame sorgente conformata all'fps della timeline (vedi `Clip::rate`).
/// `1/1` se il media non è (più) nel pool.
fn drag_timeline_len(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &MediaDrag,
) -> FrameIdx {
    let rate = project
        .media_pool
        .get(drag.media_id)
        .map(|item| vv_core::Rational::conform_rate(timeline_fps, item.meta.fps))
        .unwrap_or_else(vv_core::Rational::one);
    drag.timeline_len(rate)
}

/// Lunghezza totale del drop: i media accodati uno dopo l'altro.
fn drag_set_timeline_len(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &TimelineDrag,
) -> FrameIdx {
    match drag {
        TimelineDrag::Media(set) => set
            .items
            .iter()
            .map(|d| drag_timeline_len(project, timeline_fps, d))
            .sum(),
        TimelineDrag::Generator(g) => g.default_len(timeline_fps),
    }
}

/// Un segmento del ghost di drop: i media si accodano, quindi il ghost li
/// mostra separati.
struct DragSegment {
    offset: FrameIdx,
    len: FrameIdx,
    has_video: bool,
    has_audio: bool,
}

fn drag_set_segments(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &TimelineDrag,
) -> Vec<DragSegment> {
    let set = match drag {
        TimelineDrag::Media(set) => set,
        TimelineDrag::Generator(g) => {
            return vec![DragSegment {
                offset: 0,
                len: g.default_len(timeline_fps),
                has_video: true,
                has_audio: false,
            }];
        }
    };
    let mut offset = 0;
    set.items
        .iter()
        .filter(|d| project.media_pool.contains_key(d.media_id))
        .map(|d| {
            let len = drag_timeline_len(project, timeline_fps, d);
            let seg = DragSegment {
                offset,
                len,
                has_video: d.takes_video(&project.media_pool[d.media_id].meta),
                has_audio: d.takes_audio(&project.media_pool[d.media_id].meta),
            };
            offset += len;
            seg
        })
        .collect()
}

/// Dove va un drop: la track di sempre, una nuova (fascia sopra le Video o
/// sotto le Audio) o una track video precisa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDropTarget {
    Default,
    NewVideoTrack,
    NewAudioTrack,
    /// Track video esistente sotto al puntatore (drop di un effetto o di
    /// un media con video, quando il puntatore è su una track video già
    /// presente).
    Track(usize),
}

/// `Some((drag, frame, target))` se in questo frame è stato rilasciato
/// qualcosa trascinato dal media pool o dal pannello Effects; il chiamante
/// se ne occupa.
pub fn show_timeline(
    ui: &mut egui::Ui,
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
    state: &mut TimelineState,
    snapping_enabled: bool,
    kinetic_scroll_enabled: bool,
    // Intervalli di timeline già in cache: striscia "buffered" nel righello.
    buffered_ranges: &[(FrameIdx, FrameIdx)],
    // Intervalli di timeline delle clip servite dal proxy: striscia sulla clip.
    proxy_ranges: &[(FrameIdx, FrameIdx)],
    // Waveform caricate, per `(content_hash, stream_index)`.
    waveform_cache: &std::collections::HashMap<(u64, usize), vv_media::Waveform>,
    // La timeline è in riproduzione: durante la riproduzione la testina deve
    // sempre restare visibile, quindi la vista "volta pagina" per
    // seguirla quando esce dall'area visibile (vedi sotto).
    playback_active: bool,
) -> Option<(TimelineDrag, FrameIdx, MediaDropTarget)> {
    let mut media_drop = None;

    // Alt+scroll/pinch zooma solo col puntatore sulla timeline.
    let panel_rect = ui.available_rect_before_wrap();
    let pointer_over_panel = ui
        .input(|i| i.pointer.hover_pos())
        .is_some_and(|p| panel_rect.contains(p));
    if pointer_over_panel {
        let zoom = ui.input(|i| i.zoom_delta());
        if zoom != 1.0 {
            state.set_pixels_per_sec(state.pixels_per_sec * zoom);
        }
    }

    // Un tocco nuovo interrompe subito qualunque inerzia residua, come su
    // un vero touchpad: un click/tap (1 dito), l'inizio di un gesto di
    // scroll (2 dita, `TouchPhase::Start`) prima ancora che produca un
    // delta, o anche solo il puntatore che si muove — appoggiare le dita
    // sul touchpad senza sollevarle non genera nessun evento dedicato (il
    // sistema riporta solo variazioni, non "dita ferme"), ma un tocco vero
    // non è mai perfettamente immobile: un micro-movimento del puntatore è
    // il segnale che ci resta per accorgercene.
    let touch_started = ui.input(|i| {
        i.pointer.delta() != egui::Vec2::ZERO
            || (pointer_over_panel
                && (i.pointer.any_pressed()
                    || i.events.iter().any(|e| {
                        matches!(e, egui::Event::MouseWheel { phase: egui::TouchPhase::Start, .. })
                    })))
    });
    if touch_started {
        state.video_scroll_vel = 0.0;
        state.audio_scroll_vel = 0.0;
        state.hscroll_vel = 0.0;
    }

    let timeline_fps = project.timelines[timeline_id].fps;
    let fps = timeline_fps.as_f64();
    let px_per_frame = state.pixels_per_sec / fps.max(1.0) as f32;

    // --- pass 1: raccogli i dati da disegnare (borrow immutabile) ---
    let (track_count, track_kinds, visuals, max_end_frames) = {
        let tl = &project.timelines[timeline_id];
        let mut visuals = Vec::new();
        let mut max_end: FrameIdx = 0;
        for (track_index, track) in tl.tracks.iter().enumerate() {
            for clip in &track.clips {
                max_end = max_end.max(clip.timeline_end());
                let offline = matches!(
                    &clip.source,
                    ClipSource::Media(id) if !project.media_pool.contains_key(*id)
                );
                let (label, color) = clip_label_and_color(clip, track, offline, media_labels);
                visuals.push(ClipVisual {
                    track_index,
                    clip: std::borrow::Cow::Borrowed(clip),
                    label,
                    color,
                    locked: track.locked,
                    muted: clip.disabled || (track.kind == TrackKind::Video && track.muted),
                });
            }
        }
        let track_kinds: Vec<TrackKind> = tl.tracks.iter().map(|t| t.kind).collect();
        (tl.tracks.len(), track_kinds, visuals, max_end)
    };
    let track_labels: Vec<String> = (0..track_count)
        .map(|i| project.timelines[timeline_id].track_label(i))
        .collect();
    let track_flags: Vec<TrackFlags> = project.timelines[timeline_id]
        .tracks
        .iter()
        .map(|t| TrackFlags {
            muted: t.muted,
            solo: t.solo,
            locked: t.locked,
        })
        .collect();
    let track_locked = |track_index: usize| track_flags.get(track_index).is_some_and(|f| f.locked);
    state.drop_locked(&project.timelines[timeline_id]);

    let total_secs = (max_end_frames as f64 / fps + TRAILING_MARGIN_SECS).max(MIN_TIMELINE_SECS);
    // A zoom basso il contenuto naturale è più stretto del pannello: forziamo
    // almeno `viewport_width` così il righello arriva sempre al bordo.
    let viewport_width = (panel_rect.width() - TRACK_HEADER_WIDTH).max(1.0);
    let content_width =
        ((total_secs * state.pixels_per_sec as f64) as f32).max(viewport_width);

    let row_order = track_row_order(&track_kinds);
    let mut row_of_track = vec![0usize; track_count];
    for (row, &track_index) in row_order.iter().enumerate() {
        row_of_track[track_index] = row;
    }
    let video_count = track_kinds.iter().filter(|k| **k == TrackKind::Video).count();
    let audio_count = track_count - video_count;
    let avail_below_ruler = (panel_rect.height() - RULER_HEIGHT).max(0.0);
    let mut layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    if !kinetic_scroll_enabled {
        state.video_scroll_vel = 0.0;
        state.audio_scroll_vel = 0.0;
    }

    // Inerzia residua da uno swipe da touchpad appena finito: continua a
    // scorrere e frena, anche se nel frattempo il puntatore si è spostato.
    let video_coasted =
        apply_kinetic_scroll(&mut state.video_scroll, &mut state.video_scroll_vel, layout.video_max_scroll, dt);
    let audio_coasted =
        apply_kinetic_scroll(&mut state.audio_scroll, &mut state.audio_scroll_vel, layout.audio_max_scroll, dt);
    if video_coasted || audio_coasted {
        layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
        ui.ctx().request_repaint();
    }

    // Rotella: scroll verticale del riquadro sotto al puntatore (quello
    // orizzontale resta a Shift+rotella, come già faceva la ScrollArea).
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
        && panel_rect.contains(pos)
    {
        let local_y = pos.y - panel_rect.top();
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            let scrolled = if layout.video_pane.contains(local_y) && layout.video_max_scroll > 0.0 {
                state.video_scroll += wheel;
                state.video_scroll_vel =
                    if kinetic_scroll_enabled && dt > 0.0 { KINETIC_VELOCITY_GAIN * wheel / dt } else { 0.0 };
                true
            } else if layout.audio_pane.contains(local_y) && layout.audio_max_scroll > 0.0 {
                state.audio_scroll -= wheel;
                state.audio_scroll_vel =
                    if kinetic_scroll_enabled && dt > 0.0 { -KINETIC_VELOCITY_GAIN * wheel / dt } else { 0.0 };
                true
            } else {
                false
            };
            if scrolled {
                ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
                layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
            }
        }
    }
    let divider_height = layout.divider_height;
    let visual_height = layout.audio_pane.max;
    let pane_of = |track_index: usize| layout.pane(track_kinds[track_index]);

    // `y` locale di ogni track (indicizzata da `track_index`), coerente con
    // `clip_local_rect`.
    let row_y: Vec<f32> = (0..track_count)
        .map(|track_index| layout.row_y(row_of_track[track_index]))
        .collect();
    let track_at_y = |local_y: f32| -> usize { row_order[layout.row_at_y(local_y)] };

    let mut pending: Option<PendingAction> = None;
    // Preso da `state.volume_drag` prima che il reset più sotto lo azzeri, per
    // chiudere il gruppo di undo dopo che `apply_pending_action` ha applicato
    // l'ultimo `SetGain` del drag (vedi `VolumeDragState::group`).
    let mut volume_drag_group: Option<vv_core::GroupMark> = None;

    ui.horizontal_top(|ui| {
        // `Id::with(IdSalt)` e `Id::with(&str)` danno id diversi: serve la forma
        // che la ScrollArea usa in `begin`.
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("timeline_scroll"));
        let scroll_viewport_width =
            (ui.available_rect_before_wrap().width() - TRACK_HEADER_WIDTH).max(1.0);
        sync_timeline_scroll(
            ui.ctx(),
            scroll_id,
            state,
            fps,
            px_per_frame,
            scroll_viewport_width,
            content_width,
            playback_active,
            panel_rect,
            kinetic_scroll_enabled,
        );

        draw_track_headers(
            ui,
            &track_kinds,
            &track_labels,
            &track_flags,
            &row_order,
            &row_y,
            &layout,
            &mut state.video_pane_height,
            &mut pending,
            state.playhead,
            fps,
        );

        egui::ScrollArea::horizontal()
            .id_salt("timeline_scroll")
            // `auto_shrink` spento: altrimenti il pannello si richiude al contenuto e
            // il suo resize torna indietro.
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (rect, _resp) = ui.allocate_exact_size(
                    egui::vec2(content_width, RULER_HEIGHT),
                    egui::Sense::hover(),
                );
                let origin = rect.min;
                // Non allocato: l'altezza dei riquadri dipende dal pannello,
                // e allocarla farebbe crescere `Panel::bottom` all'infinito.
                let visual_rect = egui::Rect::from_min_size(
                    origin,
                    egui::vec2(content_width, visual_height),
                );
                let painter = ui.painter_at(visual_rect);
                let to_local = |pos: egui::Pos2| egui::pos2(pos.x - origin.x, pos.y - origin.y);
                let pane_rect = |pane: egui::Rangef| {
                    egui::Rect::from_x_y_ranges(
                        visual_rect.x_range(),
                        (origin.y + pane.min)..=(origin.y + pane.max),
                    )
                };
                let track_pane_rect = |track_index: usize| pane_rect(pane_of(track_index));
                let track_painter = |track_index: usize| {
                    painter.with_clip_rect(painter.clip_rect().intersect(track_pane_rect(track_index)))
                };
                let local_pane_rect = |track_index: usize| {
                    track_pane_rect(track_index).translate(-origin.to_vec2())
                };
                // Solo la parte della clip visibile nel suo riquadro.
                let visible_clip_rect = |v: &ClipVisual| {
                    clip_local_rect(v, px_per_frame, &row_y).intersect(local_pane_rect(v.track_index))
                };
                let press_over_a_clip = |pos: egui::Pos2| {
                    let local = to_local(pos);
                    visuals.iter().any(|v| visible_clip_rect(v).contains(local))
                };

                show_ruler(
                    ui,
                    &painter,
                    origin,
                    content_width,
                    state,
                    &visuals,
                    fps,
                    px_per_frame,
                    snapping_enabled,
                    buffered_ranges,
                    max_end_frames,
                );

                // Sfondi delle track e, sopra, un'area interagibile per click e
                // rettangolo dal vuoto. Le clip sono interagite dopo e vincono l'hit-test.
                for (row, &track_index) in row_order.iter().enumerate() {
                    let y = origin.y + row_y[track_index];
                    let track_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, y),
                        egui::vec2(content_width, ROW_HEIGHT),
                    );
                    let bg = match (track_locked(track_index), row % 2 == 0) {
                        (true, _) => egui::Color32::from_gray(42),
                        (false, true) => egui::Color32::from_gray(32),
                        (false, false) => egui::Color32::from_gray(27),
                    };
                    track_painter(track_index).rect_filled(track_rect, 0.0, bg);
                }
                let over_rows = |pos: egui::Pos2| {
                    visual_rect.x_range().contains(pos.x) && layout.is_over_rows(pos.y - origin.y)
                };
                // Anche le zone vuote: il rettangolo di selezione può
                // partire da lì.
                let marquee_area_rect = egui::Rect::from_min_max(
                    egui::pos2(origin.x, origin.y + RULER_HEIGHT),
                    egui::pos2(origin.x + content_width, origin.y + visual_height),
                );
                let pointer_over_tracks = ui
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(over_rows);
                let marquee_resp = ui.interact(
                    marquee_area_rect,
                    ui.id().with("timeline_marquee"),
                    egui::Sense::click_and_drag(),
                );

                // Interagito *dopo* `marquee_resp` per vincere l'hit-test su
                // questa fascia sottile (stesso pattern delle clip sotto).
                if divider_height > 0.0 {
                    let divider_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, origin.y + layout.video_pane.max),
                        egui::vec2(content_width, divider_height),
                    );
                    interact_divider(
                        ui,
                        &painter,
                        divider_rect,
                        ui.id().with("timeline_track_split"),
                        &layout,
                        &mut state.video_pane_height,
                    );
                }

                // `dnd_*_payload` guardano `contains_pointer`: funzionano anche se il
                // drag è partito da un altro widget. Il media con video o l'effetto
                // atterrano sulla track video sotto al puntatore.
                let drop_target = |drag: &TimelineDrag, pos: egui::Pos2| {
                    let track = track_at_y(pos.y - origin.y);
                    let has_video = match drag {
                        TimelineDrag::Generator(_) => true,
                        TimelineDrag::Media(set) => set
                            .items
                            .iter()
                            .any(|d| d.takes_video(&project.media_pool[d.media_id].meta)),
                    };
                    media_pool_drop_target(track, has_video, track_kinds[track], track_locked(track))
                };
                // Dove cade un drop: frame sotto al puntatore, con la calamita.
                let playhead = state.playhead;
                let drop_frame = |drag: &TimelineDrag, pos: egui::Pos2| {
                    let raw = (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    snap_frame(
                        raw,
                        drag_set_timeline_len(project, timeline_fps, drag),
                        &visuals,
                        &[],
                        &[playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0)
                };
                // Layer sopra alle clip, dipinte più avanti.
                let ghost_painter = painter.clone().with_layer_id(egui::LayerId::new(
                    egui::Order::Foreground,
                    ui.id().with("timeline_drop_ghost"),
                ));
                if pointer_over_tracks
                    && let Some(drag) = TimelineDrag::hovered(&marquee_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                    && let Some(target) = drop_target(&drag, pos)
                    && !drag_set_segments(project, timeline_fps, &drag).is_empty()
                {
                    let frame = drop_frame(&drag, pos);
                    // Le track dove `insert_media_clip` mette video e audio.
                    let first_row = |kind| {
                        (0..track_count)
                            .find(|&t| track_kinds[t] == kind && !track_locked(t))
                            .map(|t| origin.y + row_y[t])
                    };
                    let video_y = match target {
                        MediaDropTarget::Track(track) => Some(origin.y + row_y[track]),
                        _ => first_row(TrackKind::Video),
                    };
                    let audio_y = first_row(TrackKind::Audio);
                    for seg in drag_set_segments(project, timeline_fps, &drag) {
                        let x = origin.x + (frame + seg.offset) as f32 * px_per_frame;
                        let rows = [
                            (seg.has_video, video_y, layout.video_pane),
                            (seg.has_audio, audio_y, layout.audio_pane),
                        ];
                        for (_, y, pane) in rows.into_iter().filter(|(present, _, _)| *present) {
                            let Some(y) = y else { continue };
                            let ghost_painter = ghost_painter
                                .with_clip_rect(ghost_painter.clip_rect().intersect(pane_rect(pane)));
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(x, y),
                                egui::vec2(seg.len as f32 * px_per_frame, ROW_HEIGHT),
                            )
                            // Un filo di margine fra un segmento e il
                            // successivo: senza, i bordi combaciano e le clip
                            // accodate sembrano un blocco unico.
                            .shrink2(egui::vec2(1.0, 0.0));
                            ghost_painter.rect_filled(
                                rect,
                                4.0,
                                egui::Color32::from_rgba_unmultiplied(120, 220, 120, 90),
                            );
                            ghost_painter.rect_stroke(
                                rect,
                                4.0,
                                egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                                egui::StrokeKind::Inside,
                            );
                        }
                    }
                }
                if pointer_over_tracks
                    && let Some(drag) = TimelineDrag::released(&marquee_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                    && let Some(target) = drop_target(&drag, pos)
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, target));
                }

                // Ghost di un filtro: un ingranaggio invece del rettangolo verde,
                // dovunque sulla timeline (non solo sulle track, come sopra: un
                // filtro non atterra mai su uno spazio vuoto, ma il cursore
                // resta comunque coerente mentre ci passa sopra).
                if pointer_over_panel
                    && egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(ui.ctx())
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                {
                    paint_gear_icon(&ghost_painter, pos + egui::vec2(14.0, 14.0), 10.0, egui::Color32::WHITE);
                }

                // Stesso ghost a ingranaggio dei filtri: anche una
                // transizione (dal pannello o duplicata con Alt+drag)
                // atterra solo su una clip esistente, mai su uno spazio
                // vuoto.
                if pointer_over_panel
                    && (egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(ui.ctx())
                        || egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(ui.ctx()))
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                {
                    paint_gear_icon(&ghost_painter, pos + egui::vec2(14.0, 14.0), 10.0, egui::Color32::WHITE);
                }

                // Zone "aggiungi una nuova track": margini sopra/sotto ai
                // gruppi (altezza zero se non c'è margine, vedi sopra).
                let above_video_rect = egui::Rect::from_min_max(
                    egui::pos2(origin.x, origin.y + layout.video_pane.min),
                    egui::pos2(
                        origin.x + content_width,
                        origin.y + layout.video_rows_top.max(layout.video_pane.min),
                    ),
                );
                let above_video_resp = ui.interact(
                    above_video_rect,
                    ui.id().with("timeline_new_video_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&above_video_resp).is_some() {
                    paint_drop_zone(&painter, above_video_rect, Some(&t!("timeline.new_video_track")));
                }
                if let Some(drag) = TimelineDrag::released(&above_video_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, MediaDropTarget::NewVideoTrack));
                }

                let below_audio_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        origin.x,
                        origin.y + layout.audio_rows_bottom().min(layout.audio_pane.max),
                    ),
                    egui::pos2(origin.x + content_width, origin.y + layout.audio_pane.max),
                );
                let below_audio_resp = ui.interact(
                    below_audio_rect,
                    ui.id().with("timeline_new_audio_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&below_audio_resp).is_some_and(|d| d.is_media()) {
                    paint_drop_zone(&painter, below_audio_rect, Some(&t!("timeline.new_audio_track")));
                }
                if let Some(drag) = TimelineDrag::released(&below_audio_resp)
                    && drag.is_media()
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, MediaDropTarget::NewAudioTrack));
                }

                if marquee_resp.drag_started() {
                    if let Some(pos) = marquee_resp.interact_pointer_pos()
                        && !press_over_a_clip(pos)
                    {
                        let local = to_local(pos);
                        state.marquee = Some(MarqueeDrag {
                            start: local,
                            current: local,
                        });
                    }
                } else if marquee_resp.dragged() {
                    if let (Some(m), Some(pos)) =
                        (&mut state.marquee, marquee_resp.interact_pointer_pos())
                    {
                        m.current = to_local(pos);
                    }
                } else if marquee_resp.drag_stopped() {
                    if let Some(m) = state.marquee.take() {
                        let rect = egui::Rect::from_two_pos(m.start, m.current);
                        let hits: Vec<ClipKey> = visuals
                            .iter()
                            .filter(|v| !v.locked && visible_clip_rect(v).intersects(rect))
                            .map(|v| (v.track_index, v.clip.id))
                            .collect();
                        state.selected = expand_to_linked_groups(&visuals, hits.iter().copied());
                        state.selection_anchor = hits.first().copied();
                        state.selected_gap = None;
                        state.selected_transition = None;
                    }
                } else if marquee_resp.clicked()
                    && let Some(pos) = marquee_resp.interact_pointer_pos()
                    && !press_over_a_clip(pos)
                {
                    if row_order.is_empty() || !over_rows(pos) {
                        state.clear_selection();
                    } else {
                        // Click su un vuoto seguito da una clip: lo si seleziona.
                        let local = to_local(pos);
                        let frame = ((local.x / px_per_frame).round() as FrameIdx).max(0);
                        let track_index = track_at_y(local.y);
                        match gap_at(&visuals, track_index, frame)
                            .filter(|_| !track_locked(track_index))
                        {
                            Some((gap_start, gap_end)) => {
                                state.selected.clear();
                                state.selection_anchor = None;
                                state.selected_gap = Some((track_index, gap_start, gap_end));
                                state.selected_transition = None;
                            }
                            None => state.clear_selection(),
                        }
                    }
                }
                if let Some(m) = &state.marquee {
                    let marquee_rect = egui::Rect::from_two_pos(
                        origin + m.start.to_vec2(),
                        origin + m.current.to_vec2(),
                    );
                    painter.rect_filled(
                        marquee_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(100, 150, 255, 40),
                    );
                    painter.rect_stroke(
                        marquee_rect,
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 150, 255)),
                        egui::StrokeKind::Inside,
                    );
                }

                // Vuoto selezionato: stessa cornice di una clip selezionata.
                if let Some((track_index, gap_start, gap_end)) = state.selected_gap
                    && let Some(&row_y_val) = row_y.get(track_index)
                {
                    let y = origin.y + row_y_val;
                    let gap_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x + gap_start as f32 * px_per_frame, y + 2.0),
                        egui::vec2(
                            (gap_end - gap_start) as f32 * px_per_frame,
                            ROW_HEIGHT - 4.0,
                        ),
                    );
                    let painter = track_painter(track_index);
                    painter.rect_filled(
                        gap_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                    );
                    painter.rect_stroke(
                        gap_rect,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::WHITE),
                        egui::StrokeKind::Inside,
                    );
                }

                // Track candidata del drag, dalla posizione attuale del puntatore.
                let drag_effective_track = state.drag.as_ref().map(|d| {
                    let kind = track_kinds[d.track_index];
                    let target = ui.input(|i| i.pointer.interact_pos()).and_then(|pos| {
                        track_drag_target(
                            to_local(pos).y,
                            kind,
                            &row_order,
                            &layout,
                        )
                    });
                    match target {
                        Some(TrackDragTarget::Track(idx)) if !track_locked(idx) => {
                            EffectiveTrack::Existing(idx)
                        }
                        Some(TrackDragTarget::NewTrack) => EffectiveTrack::New(1),
                        _ => EffectiveTrack::Existing(d.track_index),
                    }
                });
                if let (Some(d), Some(EffectiveTrack::New(_))) = (&state.drag, drag_effective_track) {
                    let rect = match track_kinds[d.track_index] {
                        TrackKind::Video => above_video_rect,
                        TrackKind::Audio => below_audio_rect,
                    };
                    paint_drop_zone(&painter, rect, None);
                }

                // Vedi `drag_group_row_targets`.
                // Se una qualunque clip del gruppo finirebbe su una track
                // bloccata, il gruppo resta sulle sue track.
                let drag_group_targets: Option<Vec<(ClipId, EffectiveTrack)>> =
                    state.drag.as_ref().map(|d| {
                        let targets_for = |primary_target| {
                            drag_group_row_targets(
                                d.clip_id,
                                d.track_index,
                                primary_target,
                                &d.followers,
                                &track_kinds,
                                &row_of_track,
                                &row_order,
                                video_count,
                                track_count,
                            )
                        };
                        let targets = targets_for(drag_effective_track.unwrap());
                        if targets.iter().any(|(_, t)| {
                            matches!(t, EffectiveTrack::Existing(track) if track_locked(*track))
                        }) {
                            targets_for(EffectiveTrack::Existing(d.track_index))
                        } else {
                            targets
                        }
                    });

                // Posizione della primaria trascinata (clampata e agganciata), una volta
                // sola per tutto il gruppo e per l'anteprima durante il drag.
                let dragged_primary_new_start = state.drag.as_ref().map(|d| {
                    let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                    let raw_rounded = raw.round() as FrameIdx;
                    let len = visuals
                        .iter()
                        .find(|v| v.clip.id == d.clip_id)
                        .map(|v| v.clip.timeline_len)
                        .unwrap_or(0);
                    let (min_start, max_start) = group_drag_bounds(
                        &visuals,
                        raw_rounded,
                        drag_group_targets.as_deref().unwrap(),
                        &d.followers,
                    );
                    let candidate = raw_rounded.clamp(min_start, max_start);
                    let mut exclude = vec![d.clip_id];
                    exclude.extend(d.followers.iter().map(|(id, _, _)| *id));
                    snap_frame(
                        candidate,
                        len,
                        &visuals,
                        &exclude,
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .clamp(min_start, max_start)
                });

                // Come sopra per il bordo di un trim, che cambia anche la lunghezza.
                let trimmed_primary_new_value = state.trim.as_ref().map(|t| {
                    let raw = t.original_value as f32 + t.accum_px / px_per_frame;
                    let exclude: Vec<ClipId> = std::iter::once(t.clip_id)
                        .chain(t.followers.iter().map(|&(id, _, _, _)| id))
                        .collect();
                    let snapped = snap_frame(
                        raw.round() as FrameIdx,
                        0,
                        &visuals,
                        &exclude,
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    );
                    snapped.clamp(t.min_value, t.max_value)
                });

                // `drag`/`trim` si azzerano solo dopo il loop: le clip del gruppo
                // disegnate dopo la primaria tornerebbero per un frame alla posizione
                // iniziale.
                let mut drag_finished = false;
                let mut trim_finished = false;
                let mut fade_drag_finished = false;
                let mut transition_drag_finished = false;
                let mut crossing_drag_finished = false;
                let mut volume_drag_finished = false;
                let mut edge_cursor: Option<(egui::Pos2, EdgeCursor)> = None;
                // Il marker speculare sul vicino va disegnato dopo l'intero loop,
                // non durante l'iterazione della clip sotto il puntatore: se il
                // vicino viene dopo in `draw_order` (il caso comune, clip più
                // recenti hanno id più alti), il suo stesso `paint_clip_box` lo
                // ricoprirebbe subito — vedi il commento su `drag_finished` sopra
                // per lo stesso motivo strutturale.
                let mut pending_crossing_previews: Vec<(usize, ClipId, FadeEdge)> = Vec::new();

                // Le clip in movimento si disegnano per ultime: invadono le altre.
                let trimmed_keys: Vec<ClipKey> = state
                    .trim
                    .as_ref()
                    .map(|t| {
                        std::iter::once((t.track_index, t.clip_id))
                            .chain(t.followers.iter().map(|&(id, track, _, _)| (track, id)))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut moving_keys = trimmed_keys.clone();
                if let Some(d) = &state.drag {
                    moving_keys.push((d.track_index, d.clip_id));
                    moving_keys.extend(d.followers.iter().map(|&(id, track, _)| (track, id)));
                }
                let draw_order: Vec<&ClipVisual> = visuals
                    .iter()
                    .filter(|v| !moving_keys.contains(&(v.track_index, v.clip.id)))
                    .chain(
                        visuals
                            .iter()
                            .filter(|v| moving_keys.contains(&(v.track_index, v.clip.id))),
                    )
                    .collect();
                // Duplicando, gli originali restano visibili al loro posto.
                if state.drag.as_ref().is_some_and(|d| d.duplicate) {
                    for visual in visuals
                        .iter()
                        .filter(|v| moving_keys.contains(&(v.track_index, v.clip.id)))
                    {
                        let clip_rect = egui::Rect::from_min_size(
                            egui::pos2(
                                origin.x + visual.clip.timeline_start as f32 * px_per_frame,
                                origin.y + row_y[visual.track_index] + 2.0,
                            ),
                            egui::vec2(
                                (visual.clip.timeline_len as f32 * px_per_frame).max(2.0),
                                ROW_HEIGHT - 4.0,
                            ),
                        );
                        let painter = track_painter(visual.track_index);
                        painter.rect_filled(clip_rect, 4.0, visual.color);
                        painter.rect_stroke(
                            clip_rect,
                            4.0,
                            egui::Stroke::new(1.0, egui::Color32::from_gray(15)),
                            egui::StrokeKind::Inside,
                        );
                        painter.text(
                            clip_rect.left_top() + egui::vec2(4.0, 2.0),
                            egui::Align2::LEFT_TOP,
                            &visual.label,
                            egui::FontId::proportional(12.0),
                            egui::Color32::BLACK,
                        );
                    }
                }
                for visual in draw_order {
                    let painter = track_painter(visual.track_index);
                    let is_trimming_this = trimmed_keys.contains(&(visual.track_index, visual.clip.id));
                    let (display_start, display_len) = display_range(
                        visual,
                        state,
                        is_trimming_this,
                        trimmed_primary_new_value,
                        dragged_primary_new_start,
                    );

                    let x = origin.x + display_start as f32 * px_per_frame;
                    // Durante un drag che cambia track, l'anteprima di ogni
                    // clip del gruppo segue il proprio target (vedi
                    // `drag_group_targets`) invece della track di partenza.
                    let this_target = drag_group_targets
                        .as_ref()
                        .and_then(|targets| targets.iter().find(|(id, _)| *id == visual.clip.id));
                    let y = match this_target {
                        Some((_, EffectiveTrack::Existing(track))) => origin.y + row_y[*track],
                        // Ogni "profondità" impila un'altra riga oltre al
                        // bordo attuale (vedi `EffectiveTrack::New`).
                        Some((_, EffectiveTrack::New(depth))) => match track_kinds[visual.track_index] {
                            TrackKind::Video => {
                                origin.y + layout.video_rows_top - *depth as f32 * ROW_HEIGHT
                            }
                            TrackKind::Audio => {
                                origin.y + layout.audio_rows_bottom() + (*depth - 1) as f32 * ROW_HEIGHT
                            }
                        },
                        None => origin.y + row_y[visual.track_index],
                    };
                    let w = (display_len as f32 * px_per_frame).max(2.0);
                    let clip_rect = egui::Rect::from_min_size(
                        egui::pos2(x, y + 2.0),
                        egui::vec2(w, ROW_HEIGHT - 4.0),
                    );

                    let id = ui.id().with("clip").with(visual.clip.id.0);
                    let sense = if visual.locked {
                        egui::Sense::hover()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let resp = ui.interact(
                        clip_rect.intersect(track_pane_rect(visual.track_index)),
                        id,
                        sense,
                    );

                    let is_selected = state.selected.contains(&(visual.track_index, visual.clip.id));
                    paint_clip_box(&painter, clip_rect, visual, is_selected);

                    // Filtro trascinato dal pannello Effects: solo le clip video,
                    // non bloccate, lo accettano (niente spazi vuoti o nuove
                    // track, a differenza di Generator/Media). La guardia
                    // `has_payload_of_type` prima di `dnd_release_payload` non è
                    // ridondante: quest'ultimo scarta il payload globale anche
                    // quando il tipo non combacia (side effect di egui, vedi
                    // `TimelineDrag::released`) — senza, trascinare una
                    // `TransitionKind` sulla stessa clip la perderebbe qui,
                    // prima ancora che il blocco sotto la veda.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(ui.ctx())
                    {
                        if resp.dnd_hover_payload::<vv_core::FilterKind>().is_some() {
                            painter.rect_stroke(
                                clip_rect,
                                4.0,
                                egui::Stroke::new(3.0, FILTER_HIGHLIGHT_COLOR),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(filter) = resp.dnd_release_payload::<vv_core::FilterKind>() {
                            pending = Some(PendingAction::ApplyFilter {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                filter: *filter,
                            });
                        }
                    }

                    // Transizione trascinata dal pannello Effects: come i filtri,
                    // solo clip video non bloccate — ma in più solo vicino a un
                    // bordo (mai al centro, mai su uno spazio vuoto o una nuova
                    // track): il lato più vicino al puntatore decide se diventa
                    // `transition_in` o `transition_out`. Stessa guardia di
                    // sopra, stesso motivo.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(ui.ctx())
                    {
                        let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(clip_rect.width() / 2.0);
                        let hover_edge = ui.input(|i| i.pointer.hover_pos())
                            .and_then(|pos| transition_drop_edge(pos, clip_rect, drop_zone_px));
                        if resp.dnd_hover_payload::<vv_core::TransitionKind>().is_some()
                            && let Some(edge) = hover_edge
                        {
                            let x = match edge {
                                FadeEdge::In => clip_rect.left() + drop_zone_px,
                                FadeEdge::Out => clip_rect.right() - drop_zone_px,
                            };
                            let is_crossing = has_neighbor(&visuals, visual.track_index, visual.clip.id, edge);
                            paint_transition_marker(&painter, clip_rect, edge, x, false, is_crossing);
                            if is_crossing {
                                pending_crossing_previews.push((visual.track_index, visual.clip.id, edge));
                            }
                        }
                        if let Some(kind) = resp.dnd_release_payload::<vv_core::TransitionKind>()
                            && let Some(edge) = hover_edge
                        {
                            pending = Some(PendingAction::ApplyTransition {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                edge,
                                kind: *kind,
                            });
                        }
                    }

                    // Alt+drag di una transizione esistente (duplicazione, vedi
                    // `begin_transition_duplicate_drag`): stesso payload di un
                    // drop dal pannello Effects ma con `vv_core::Transition`
                    // intero invece di `TransitionKind`, per mantenerne i
                    // parametri (durata, direzione, ease, curva) invece di
                    // ripartire dai default. Stessa guardia, stesso motivo.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(ui.ctx())
                    {
                        let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(clip_rect.width() / 2.0);
                        let hover_edge = ui.input(|i| i.pointer.hover_pos())
                            .and_then(|pos| transition_drop_edge(pos, clip_rect, drop_zone_px));
                        if resp.dnd_hover_payload::<vv_core::Transition>().is_some()
                            && let Some(edge) = hover_edge
                        {
                            let x = match edge {
                                FadeEdge::In => clip_rect.left() + drop_zone_px,
                                FadeEdge::Out => clip_rect.right() - drop_zone_px,
                            };
                            let is_crossing = has_neighbor(&visuals, visual.track_index, visual.clip.id, edge);
                            paint_transition_marker(&painter, clip_rect, edge, x, false, is_crossing);
                            if is_crossing {
                                pending_crossing_previews.push((visual.track_index, visual.clip.id, edge));
                            }
                        }
                        if let Some(transition) = resp.dnd_release_payload::<vv_core::Transition>()
                            && let Some(edge) = hover_edge
                        {
                            pending = Some(PendingAction::DuplicateTransition {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                edge,
                                transition: (*transition).clone(),
                            });
                        }
                    }

                    // Waveform: massimo dei bin per colonna, forma indipendente dallo zoom.
                    if track_kinds[visual.track_index] == TrackKind::Audio
                        && let ClipSource::Media(media_id) = &visual.clip.source
                        && let Some(item) = project.media_pool.get(*media_id)
                        && let Some(wf) = waveform_cache
                            .get(&(item.content_hash, visual.clip.audio_stream_index))
                    {
                        // Durante un trim la clip disegnata copre un'altra
                        // fascia di sorgente: senza rimapparla la forma
                        // d'onda si stirerebbe invece di essere tagliata.
                        let (wave_start, wave_end) = if is_trimming_this {
                            (display_start, display_start + display_len)
                        } else {
                            (visual.clip.timeline_start, visual.clip.timeline_end())
                        };
                        let fps = timeline_fps.as_f64();
                        draw_clip_waveform(
                            &painter,
                            clip_rect,
                            &wf.peaks,
                            visual.clip.media_secs_at(wave_start, fps),
                            visual.clip.media_secs_at(wave_end, fps),
                            item.meta.fps.as_f64(),
                            wf.audio_duration_secs,
                            painter.clip_rect(),
                            &visual.clip.effects.gain_db,
                        );
                    }

                    let is_proxy_backed = proxy_ranges.iter().any(|&(s, e)| {
                        s < visual.clip.timeline_end() && e >= visual.clip.timeline_start
                    });
                    paint_clip_overlay(&painter, clip_rect, visual, is_proxy_backed);

                    // Riga del volume: sottile linea orizzontale trascinabile
                    // in verticale, centrata a 0 dB (vedi `gain_offset`). Solo
                    // se il gain non è keyframato: una riga piatta mentirebbe
                    // sulla curva reale, che si edita dal pannello proprietà.
                    if track_kinds[visual.track_index] == TrackKind::Audio
                        && visual.clip.effects.gain_db.is_constant()
                    {
                        let dragging = state
                            .volume_drag
                            .as_ref()
                            .is_some_and(|d| d.clip_id == visual.clip.id);
                        paint_gain_line(
                            &painter,
                            clip_rect,
                            gain_line_y(visual.clip.effects.gain_db.default, clip_rect),
                            dragging,
                        );
                    }

                    // Handle di fade-in/fade-out: presenti sempre se la
                    // dissolvenza è già impostata, altrimenti solo mentre la
                    // clip è sotto il mouse (per afferrarli dall'angolo).
                    let (fade_in_preview, fade_out_preview) = fade_preview(state, visual, px_per_frame);
                    let fade_in_dragging = state
                        .fade_drag
                        .as_ref()
                        .is_some_and(|d| d.clip_id == visual.clip.id && d.edge == FadeEdge::In);
                    let fade_out_dragging = state
                        .fade_drag
                        .as_ref()
                        .is_some_and(|d| d.clip_id == visual.clip.id && d.edge == FadeEdge::Out);
                    let show_fades = !visual.locked && clip_rect.width() >= MIN_FADE_CLIP_WIDTH_PX;
                    let fade_in_x = clip_rect.left()
                        + (fade_in_preview as f32 * px_per_frame).min(clip_rect.width());
                    let fade_out_x = clip_rect.right()
                        - (fade_out_preview as f32 * px_per_frame).min(clip_rect.width());
                    if show_fades && (visual.clip.fade_in > 0 || fade_in_dragging || resp.hovered()) {
                        paint_fade_wedge(&painter, clip_rect, clip_rect.left(), fade_in_x, fade_in_dragging);
                    }
                    if show_fades && (visual.clip.fade_out > 0 || fade_out_dragging || resp.hovered()) {
                        paint_fade_wedge(&painter, clip_rect, clip_rect.right(), fade_out_x, fade_out_dragging);
                    }
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                        && (fade_in_dragging || fade_out_dragging)
                    {
                        let frames = if fade_in_dragging { fade_in_preview } else { fade_out_preview };
                        paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                    }

                    // Marker delle transizioni già impostate (bordo singolo o
                    // crossing): il drop dal pannello Effects (sopra) le rende
                    // persistenti, da qui in poi vivono come l'handle di fade —
                    // sempre visibili, l'estremità (durata) trascinabile.
                    let track = &project.timelines[timeline_id].tracks[visual.track_index];
                    let left_marker = edge_marker(track, state, visual, px_per_frame, FadeEdge::In);
                    let right_marker = edge_marker(track, state, visual, px_per_frame, FadeEdge::Out);
                    let transition_in_x = left_marker.as_ref().map(|m| {
                        clip_rect.left() + (m.duration as f32 * px_per_frame).min(clip_rect.width())
                    });
                    let transition_out_x = right_marker.as_ref().map(|m| {
                        clip_rect.right() - (m.duration as f32 * px_per_frame).min(clip_rect.width())
                    });
                    if let (Some(m), Some(x)) = (&left_marker, transition_in_x) {
                        let selected = state.selected_transition == Some(m.selection);
                        paint_transition_marker(&painter, clip_rect, FadeEdge::In, x, selected, m.is_crossing);
                    }
                    if let (Some(m), Some(x)) = (&right_marker, transition_out_x) {
                        let selected = state.selected_transition == Some(m.selection);
                        paint_transition_marker(&painter, clip_rect, FadeEdge::Out, x, selected, m.is_crossing);
                    }
                    // Overlay con la durata durante il drag della sua
                    // estremità: comportamento generale (vedi
                    // `paint_duration_overlay`), non solo per il fade sopra.
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                        if let Some(d) = state.transition_drag.as_ref().filter(|d| d.clip_id == visual.clip.id) {
                            let frames = transition_drag_value(d, visual.clip.timeline_len, px_per_frame);
                            paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                        } else if let Some(d) = state.crossing_drag.as_ref().filter(|d| {
                            [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(d.track_index, d.left_clip)
                            })
                        }) {
                            let frames = crossing_drag_value(d, px_per_frame);
                            paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                        }
                    }

                    // Zone ridotte per le clip molto strette, altrimenti
                    // l'intera clip sarebbe "solo bordi" e non si potrebbe più
                    // spostare (Move) col drag normale dal centro.
                    let adjacent = |at: FrameIdx, edge: TrimEdge| {
                        visuals
                            .iter()
                            .find(|v| {
                                v.track_index == visual.track_index
                                    && v.clip.id != visual.clip.id
                                    && match edge {
                                        TrimEdge::Start => v.clip.timeline_end() == at,
                                        TrimEdge::End => v.clip.timeline_start == at,
                                    }
                            })
                            .map(|v| (v.track_index, v.clip.id))
                    };
                    let zones = edge_zones(
                        clip_rect.width(),
                        adjacent(visual.clip.timeline_start, TrimEdge::Start),
                        adjacent(visual.clip.timeline_end(), TrimEdge::End),
                    );
                    let edge_at = |pos: egui::Pos2| zones.at(pos.x - clip_rect.left());
                    let volume_hit = |pos: egui::Pos2| {
                        track_kinds[visual.track_index] == TrackKind::Audio
                            && visual.clip.effects.gain_db.is_constant()
                            && volume_line_hit(
                                pos,
                                clip_rect,
                                gain_line_y(visual.clip.effects.gain_db.default, clip_rect),
                            )
                    };
                    if resp.hovered()
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.transition_drag.is_none()
                        && state.crossing_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && transition_handle_at(pos, clip_rect, transition_in_x, transition_out_x).is_some()
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if resp.hovered()
                        && show_fades
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.fade_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && fade_zone_at(pos, clip_rect, fade_in_x, fade_out_x).is_some()
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if resp.hovered()
                        && !visual.locked
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && let Some(zone) = edge_at(pos)
                    {
                        edge_cursor = Some((pos, EdgeCursor::from_zone(zone)));
                    } else if resp.hovered()
                        && !visual.locked
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.volume_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && volume_hit(pos)
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }

                    let marker_at = |edge: FadeEdge| match edge {
                        FadeEdge::In => left_marker.as_ref(),
                        FadeEdge::Out => right_marker.as_ref(),
                    };
                    if resp.drag_started() {
                        // `press_origin` e non la posizione attuale: egui dichiara il drag dopo un
                        // piccolo movimento, e verso l'interno si sarebbe già usciti dalla zona
                        // del bordo.
                        let press_pos = ui.input(|i| i.pointer.press_origin());
                        let transition_handle =
                            press_pos.and_then(|p| transition_handle_at(p, clip_rect, transition_in_x, transition_out_x));
                        // Alt+drag sul corpo (non sulla maniglia) di un marker già
                        // presente duplica invece di ridimensionare — la maniglia
                        // resta prioritaria, come il fade ignora Alt sulla sua.
                        let transition_duplicate_edge = if ui.input(|i| i.modifiers.alt) {
                            press_pos.and_then(|p| {
                                if transition_in_x.is_some_and(|x| transition_body_hit(p, clip_rect, FadeEdge::In, x)) {
                                    Some(FadeEdge::In)
                                } else if transition_out_x
                                    .is_some_and(|x| transition_body_hit(p, clip_rect, FadeEdge::Out, x))
                                {
                                    Some(FadeEdge::Out)
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        };
                        let fade_zone = if show_fades {
                            press_pos.and_then(|p| fade_zone_at(p, clip_rect, fade_in_x, fade_out_x))
                        } else {
                            None
                        };
                        match transition_handle.and_then(|edge| marker_at(edge).map(|m| (edge, m))) {
                            Some((edge, m)) => match m.selection {
                                TransitionSelection::Crossing(track_index, left_clip) => {
                                    begin_crossing_drag(state, project, timeline_id, track_index, left_clip, edge);
                                }
                                TransitionSelection::Edge(..) => begin_transition_drag(state, visual, edge),
                            },
                            None => match transition_duplicate_edge.and_then(marker_at) {
                            Some(m) => begin_transition_duplicate_drag(state, &resp, project, timeline_id, visual, m),
                            None => match fade_zone {
                            Some(edge) => begin_fade_drag(state, visual, edge),
                            None => match press_pos.and_then(edge_at) {
                                Some(zone) => begin_trim(state, &visuals, project, visual, zone),
                                None if press_pos.is_some_and(volume_hit) => {
                                    begin_volume_drag(state, history, visual)
                                }
                                None => {
                                    begin_drag(state, &visuals, visual, ui.input(|i| i.modifiers.alt))
                                }
                            },
                            },
                            },
                        }
                    } else if resp.dragged() {
                        if let Some(td) = &mut state.transition_drag
                            && td.clip_id == visual.clip.id
                        {
                            td.accum_px += resp.drag_delta().x;
                        } else if let Some(cd) = &mut state.crossing_drag
                            && [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(cd.track_index, cd.left_clip)
                            })
                        {
                            cd.accum_px += resp.drag_delta().x;
                        } else if let Some(fd) = &mut state.fade_drag
                            && fd.clip_id == visual.clip.id
                        {
                            fd.accum_px += resp.drag_delta().x;
                        } else if let Some(vd) = &mut state.volume_drag
                            && vd.clip_id == visual.clip.id
                        {
                            vd.accum_px += resp.drag_delta().y;
                            // A differenza di fade/trim/move, si applica già qui,
                            // a ogni frame di drag (vedi `VolumeDragState`).
                            pending = Some(PendingAction::SetGain {
                                track_index: vd.track_index,
                                clip_id: vd.clip_id,
                                new_value: volume_drag_value(vd, clip_rect.height() / 2.0),
                            });
                        } else if let Some(t) = &mut state.trim
                            && t.clip_id == visual.clip.id
                        {
                            t.accum_px += resp.drag_delta().x;
                        } else if let Some(d) = &mut state.drag
                            && d.clip_id == visual.clip.id
                        {
                            d.accum_px += resp.drag_delta().x;
                        }
                    } else if resp.drag_stopped() {
                        if let Some(td) = &state.transition_drag
                            && td.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetTransitionDuration {
                                track_index: td.track_index,
                                clip_id: td.clip_id,
                                edge: td.edge,
                                new_value: transition_drag_value(td, visual.clip.timeline_len, px_per_frame),
                            });
                            transition_drag_finished = true;
                        } else if let Some(cd) = &state.crossing_drag
                            && [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(cd.track_index, cd.left_clip)
                            })
                        {
                            pending = Some(PendingAction::SetCrossingDuration {
                                track_index: cd.track_index,
                                left_clip: cd.left_clip,
                                new_value: crossing_drag_value(cd, px_per_frame),
                            });
                            crossing_drag_finished = true;
                        } else if state.transition_duplicate_drag == Some(visual.clip.id) {
                            // Il drop vero (se c'è stato, su un'altra clip) è già
                            // gestito da `dnd_release_payload` in quella clip;
                            // qui resta solo da chiudere lo stato locale.
                            state.transition_duplicate_drag = None;
                        } else if let Some(fd) = &state.fade_drag
                            && fd.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetFade {
                                track_index: fd.track_index,
                                clip_id: fd.clip_id,
                                edge: fd.edge,
                                new_value: fade_drag_value(fd, visual.clip.timeline_len, px_per_frame),
                            });
                            fade_drag_finished = true;
                        } else if let Some(vd) = &state.volume_drag
                            && vd.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetGain {
                                track_index: vd.track_index,
                                clip_id: vd.clip_id,
                                new_value: volume_drag_value(vd, clip_rect.height() / 2.0),
                            });
                            volume_drag_group = Some(vd.group);
                            volume_drag_finished = true;
                        } else if let Some(t) = &state.trim
                            && t.clip_id == visual.clip.id
                        {
                            pending = Some(finish_trim(t, visual, &visuals, trimmed_primary_new_value));
                            trim_finished = true;
                        } else if let Some(d) = &state.drag
                            && d.clip_id == visual.clip.id
                        {
                            pending = Some(finish_drag(
                                d,
                                drag_group_targets.as_deref().unwrap(),
                                &track_kinds,
                                dragged_primary_new_start,
                            ));
                            drag_finished = true;
                        }
                    } else if resp.clicked() {
                        let clicked_transition = resp.interact_pointer_pos().and_then(|pos| {
                            if transition_in_x.is_some_and(|x| transition_body_hit(pos, clip_rect, FadeEdge::In, x)) {
                                left_marker.as_ref()
                            } else if transition_out_x
                                .is_some_and(|x| transition_body_hit(pos, clip_rect, FadeEdge::Out, x))
                            {
                                right_marker.as_ref()
                            } else {
                                None
                            }
                        });
                        if let Some(m) = clicked_transition {
                            // Sostituisce qualunque selezione di clip, anche multipla.
                            state.selected.clear();
                            state.selection_anchor = None;
                            state.selected_gap = None;
                            state.selected_transition = Some(m.selection);
                        } else {
                            let modifiers = click_modifiers(ui.input(|i| i.modifiers));
                            let (selected, anchor) = apply_click_selection(
                                &state.selected,
                                state.selection_anchor,
                                (visual.track_index, visual.clip.id),
                                modifiers,
                                &visuals,
                                px_per_frame,
                                &row_y,
                            );
                            state.selected = expand_to_linked_groups(&visuals, selected);
                            state.selection_anchor = anchor;
                            state.selected_gap = None;
                            state.selected_transition = None;
                        }
                    }

                    resp.context_menu(|ui| {
                        if visual.clip.linked_group.is_some() {
                            if ui.button(t!("timeline.unlink")).clicked() {
                                pending =
                                    Some(PendingAction::Unlink(visual.track_index, visual.clip.id));
                                ui.close();
                            }
                        } else if state.selected.len() >= 2 {
                            if ui.button(t!("timeline.link")).clicked() {
                                pending =
                                    Some(PendingAction::Link(state.selected.iter().copied().collect()));
                                ui.close();
                            }
                        } else {
                            ui.label(t!("timeline.link_hint"));
                        }
                    });
                }
                // Vedi doc di `pending_crossing_previews`: solo ora, a
                // disegno di tutte le clip concluso, nessun `paint_clip_box`
                // successivo può più ricoprirlo.
                for (track_index, clip_id, edge) in pending_crossing_previews {
                    paint_mirrored_marker_on_neighbor(
                        &track_painter(track_index),
                        &visuals,
                        origin,
                        &row_y,
                        px_per_frame,
                        track_index,
                        clip_id,
                        edge,
                    );
                }
                // Azzerati solo ora, non con un `.take()` a metà del loop
                // sopra — vedi il commento su `drag_finished`/`trim_finished`.
                if drag_finished {
                    state.drag = None;
                }
                if trim_finished {
                    state.trim = None;
                }
                if fade_drag_finished {
                    state.fade_drag = None;
                }
                if transition_drag_finished {
                    state.transition_drag = None;
                }
                if crossing_drag_finished {
                    state.crossing_drag = None;
                }
                if volume_drag_finished {
                    state.volume_drag = None;
                }
                if let Some(t) = &state.trim
                    && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                {
                    let cursor = match (t.roll, t.edge) {
                        (true, _) => EdgeCursor::Roll,
                        (false, TrimEdge::Start) => EdgeCursor::TrimStart,
                        (false, TrimEdge::End) => EdgeCursor::TrimEnd,
                    };
                    edge_cursor = Some((pos, cursor));
                }
                if let Some((pos, cursor)) = edge_cursor {
                    paint_edge_cursor(ui.ctx(), pos, cursor);
                }

                paint_playhead(&painter, origin, state.playhead as f32 * px_per_frame, visual_height);

                // Sul bordo destro visibile, non su quello del contenuto.
                let scrollbar_x = egui::Rangef::new(
                    ui.clip_rect().right() - PANE_SCROLLBAR_WIDTH - 2.0,
                    ui.clip_rect().right() - 2.0,
                );
                let scrollbar_rect = |pane: egui::Rangef| {
                    egui::Rect::from_x_y_ranges(
                        scrollbar_x,
                        (origin.y + pane.min + 2.0)..=(origin.y + pane.max - 2.0),
                    )
                };
                let video_max = layout.video_max_scroll;
                if let Some(offset) = pane_scrollbar(
                    ui,
                    &painter,
                    scrollbar_rect(layout.video_pane),
                    ui.id().with("timeline_video_vscroll"),
                    video_max - state.video_scroll,
                    video_max,
                ) {
                    state.video_scroll = video_max - offset;
                }
                if let Some(offset) = pane_scrollbar(
                    ui,
                    &painter,
                    scrollbar_rect(layout.audio_pane),
                    ui.id().with("timeline_audio_vscroll"),
                    state.audio_scroll,
                    layout.audio_max_scroll,
                ) {
                    state.audio_scroll = offset;
                }
            });

        });

    if let Some(action) = pending {
        apply_pending_action(project, history, state, timeline_id, action);
    }
    if let Some(mark) = volume_drag_group {
        history.end_group(mark);
    }

    media_drop
}

/// Inizio e lunghezza con cui disegnare `visual`: durante un trim o un drag
/// la posizione di anteprima, altrimenti quella reale.
fn display_range(
    visual: &ClipVisual,
    state: &TimelineState,
    is_trimming_this: bool,
    trimmed_primary_new_value: Option<FrameIdx>,
    dragged_primary_new_start: Option<FrameIdx>,
) -> (FrameIdx, FrameIdx) {
    if is_trimming_this
        && let (Some(t), Some(primary_value)) = (&state.trim, trimmed_primary_new_value)
    {
        let (offset, edge) = t
            .followers
            .iter()
            .find(|&&(id, track, _, _)| id == visual.clip.id && track == visual.track_index)
            .map_or((0, t.edge), |&(_, _, offset, edge)| (offset, edge));
        let new_value = primary_value + offset;
        match edge {
            TrimEdge::Start => (new_value, (visual.clip.timeline_end() - new_value).max(1)),
            TrimEdge::End => (
                visual.clip.timeline_start,
                (new_value - visual.clip.timeline_start).max(1),
            ),
        }
    } else {
        let start = match (&state.drag, dragged_primary_new_start) {
            (Some(d), Some(new_start)) if d.clip_id == visual.clip.id => new_start,
            (Some(d), Some(new_start)) => match d
                .followers
                .iter()
                .find(|(id, track, _)| *id == visual.clip.id && *track == visual.track_index)
            {
                Some((_, _, offset)) => new_start + offset,
                None => visual.clip.timeline_start,
            },
            _ => visual.clip.timeline_start,
        };
        (start, visual.clip.timeline_len)
    }
}

/// Riempimento e bordo di una clip.
fn paint_clip_box(painter: &egui::Painter, clip_rect: egui::Rect, visual: &ClipVisual, is_selected: bool) {
    let stroke = if is_selected {
        egui::Stroke::new(2.0, egui::Color32::WHITE)
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_gray(15))
    };
    let fill = if visual.muted {
        egui::Color32::from_gray(58)
    } else {
        visual.color
    };
    painter.rect_filled(clip_rect, 4.0, fill);
    painter.rect_stroke(clip_rect, 4.0, stroke, egui::StrokeKind::Inside);
}

/// Etichetta, badge "disattivata", icona di collegamento e velo delle track
/// bloccate, sopra alla waveform.
fn paint_clip_overlay(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    visual: &ClipVisual,
    is_proxy_backed: bool,
) {
    let label_offset_y = if is_proxy_backed {
        paint_proxy_strip(painter, clip_rect);
        2.0 + PROXY_STRIP_HEIGHT
    } else {
        2.0
    };
    let mut label_pos = clip_rect.left_top() + egui::vec2(4.0, label_offset_y);
    if visual.clip.disabled {
        paint_disabled_badge(painter, label_pos);
        label_pos.x += DISABLED_BADGE_SIZE + 4.0;
    }
    painter.text(
        label_pos,
        egui::Align2::LEFT_TOP,
        &visual.label,
        egui::FontId::proportional(12.0),
        if visual.muted {
            egui::Color32::from_gray(185)
        } else {
            egui::Color32::BLACK
        },
    );
    if visual.clip.linked_group.is_some() {
        // Due anelli a mano: su alcune piattaforme (Asahi) i font di egui non
        // hanno 🔗.
        let center = clip_rect.right_top() + egui::vec2(-9.0, 8.0);
        let ring_color = if visual.muted {
            egui::Color32::from_gray(185)
        } else {
            egui::Color32::BLACK
        };
        let ring_stroke = egui::Stroke::new(1.3, ring_color);
        painter.circle_stroke(center + egui::vec2(-2.5, 0.0), 3.5, ring_stroke);
        painter.circle_stroke(center + egui::vec2(2.5, 0.0), 3.5, ring_stroke);
    }
    if visual.locked {
        painter.rect_filled(
            clip_rect,
            4.0,
            egui::Color32::from_rgba_unmultiplied(70, 70, 70, 140),
        );
    }
}

/// Ombra triangolare della dissolvenza (dall'angolo verso l'handle) più il
/// pallino dell'handle stesso. `x` è la posizione corrente dell'handle,
/// `corner_x` l'angolo (sx per il fade-in, dx per il fade-out) da cui parte
/// il triangolo.
fn paint_fade_wedge(painter: &egui::Painter, clip_rect: egui::Rect, corner_x: f32, x: f32, dragging: bool) {
    let top = clip_rect.top();
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(corner_x, top),
            egui::pos2(x, top),
            egui::pos2(corner_x, clip_rect.bottom()),
        ],
        egui::Color32::from_black_alpha(110),
        egui::Stroke::NONE,
    ));
    let center = egui::pos2(x, top + FADE_HANDLE_ZONE_HEIGHT * 0.5);
    let radius = if dragging { FADE_HANDLE_RADIUS + 1.0 } else { FADE_HANDLE_RADIUS };
    painter.circle_filled(center, radius, egui::Color32::WHITE);
    painter.circle_stroke(center, radius, egui::Stroke::new(1.0, egui::Color32::from_gray(40)));
}

/// Overlay con la durata (dissolvenza, transizione singola o crossing)
/// vicino al puntatore, durante il drag della sua estremità: stesso layer
/// "sempre sopra" del cursore di trim. Comportamento generale, non solo
/// per le dissolvenze: qualunque estremità trascinabile lo mostra.
fn paint_duration_overlay(ctx: &egui::Context, pos: egui::Pos2, frames: FrameIdx, fps: f64) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("timeline_duration_overlay"),
    ));
    let text = format!("+{}", format_duration(frames, fps));
    let text_pos = pos + egui::vec2(12.0, 14.0);
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), egui::Color32::WHITE);
    let bg = egui::Rect::from_min_size(text_pos, galley.size()).expand(3.0);
    painter.rect_filled(bg, 3.0, egui::Color32::from_black_alpha(200));
    painter.galley(text_pos, galley, egui::Color32::WHITE);
}

/// `S:FF`: durata in secondi e frame residui, non una posizione di
/// timeline (niente ore/minuti, queste durate sono sempre brevi).
fn format_duration(frames: FrameIdx, fps: f64) -> String {
    let nominal = (fps.round() as i64).max(1);
    let frames = frames.max(0);
    let (secs, f) = (frames / nominal, frames % nominal);
    format!("{secs}:{f:02}")
}

/// Comincia il trim di `visual` dalla zona di bordo `zone`, con le clip che lo
/// seguono.
fn begin_trim(
    state: &mut TimelineState,
    visuals: &[ClipVisual],
    project: &Project,
    visual: &ClipVisual,
    zone: EdgeZone,
) {
    let edge = zone.edge();
    let key = (visual.track_index, visual.clip.id);
    let others: Vec<(ClipKey, TrimEdge)> = match zone {
        EdgeZone::Trim(_) => drag_group_for(&state.selected, visuals, key)
            .into_iter()
            .filter(|k| *k != key)
            .map(|k| (k, edge))
            .collect(),
        // Solo le due clip a contatto, ognuna col suo gruppo collegato.
        EdgeZone::Roll { neighbor, .. } => {
            let opposite = match edge {
                TrimEdge::Start => TrimEdge::End,
                TrimEdge::End => TrimEdge::Start,
            };
            expand_to_linked_groups(visuals, [key])
                .into_iter()
                .filter(|k| *k != key)
                .map(|k| (k, edge))
                .chain(
                    expand_to_linked_groups(visuals, [neighbor])
                        .into_iter()
                        .map(|k| (k, opposite)),
                )
                .collect()
        }
    };
    let (min_value, max_value, followers) =
        combined_trim_range(visuals, project, key, edge, &others);
    let original_value = match edge {
        TrimEdge::Start => visual.clip.timeline_start,
        TrimEdge::End => visual.clip.timeline_end(),
    };
    state.trim = Some(TrimState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
        min_value,
        max_value: max_value.max(min_value),
        followers,
        roll: matches!(zone, EdgeZone::Roll { .. }),
    });
}

/// Comincia a trascinare `visual` insieme al suo gruppo di selezione.
fn begin_drag(state: &mut TimelineState, visuals: &[ClipVisual], visual: &ClipVisual, duplicate: bool) {
    let drag_group = drag_group_for(&state.selected, visuals, (visual.track_index, visual.clip.id));
    state.selected = drag_group.clone();
    state.selection_anchor = Some((visual.track_index, visual.clip.id));

    let others: Vec<ClipKey> = drag_group.into_iter().collect();
    let (_, _, followers) =
        combined_drag_range(visuals, visual.track_index, visual.clip.id, &others);

    state.drag = Some(DragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        original_start: visual.clip.timeline_start,
        accum_px: 0.0,
        followers,
        duplicate,
    });
}

/// Comincia a trascinare l'handle di fade-in/fade-out di `visual`: nessun
/// gruppo né vicino coinvolto, è sempre locale alla clip.
fn begin_fade_drag(state: &mut TimelineState, visual: &ClipVisual, edge: FadeEdge) {
    let original_value = match edge {
        FadeEdge::In => visual.clip.fade_in,
        FadeEdge::Out => visual.clip.fade_out,
    };
    state.fade_drag = Some(FadeDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
    });
}

/// Valore (in frame, clampato alla durata della clip) dell'anteprima di un
/// fade drag in corso: trascinare l'handle di fade-in verso destra allunga
/// `fade_in`, trascinare quello di fade-out verso sinistra allunga
/// `fade_out` — verso opposti sull'asse X per lo stesso segno di `accum_px`.
fn fade_drag_value(d: &FadeDragState, clip_len: FrameIdx, px_per_frame: f32) -> FrameIdx {
    let signed_delta = match d.edge {
        FadeEdge::In => d.accum_px,
        FadeEdge::Out => -d.accum_px,
    };
    (d.original_value as f32 + signed_delta / px_per_frame)
        .round()
        .clamp(0.0, clip_len as f32) as FrameIdx
}

/// `(fade_in, fade_out)` da mostrare per `visual`: l'anteprima del drag in
/// corso se lo riguarda, altrimenti i valori già salvati.
fn fade_preview(state: &TimelineState, visual: &ClipVisual, px_per_frame: f32) -> (FrameIdx, FrameIdx) {
    let mut fade_in = visual.clip.fade_in;
    let mut fade_out = visual.clip.fade_out;
    if let Some(d) = &state.fade_drag
        && d.clip_id == visual.clip.id
    {
        let value = fade_drag_value(d, visual.clip.timeline_len, px_per_frame);
        match d.edge {
            FadeEdge::In => fade_in = value,
            FadeEdge::Out => fade_out = value,
        }
    }
    (fade_in, fade_out)
}

/// Handle di fade sotto `pos`, solo nella banda in alto alla clip: sotto
/// resta il trim/roll del bordo esistente.
fn fade_zone_at(pos: egui::Pos2, clip_rect: egui::Rect, fade_in_x: f32, fade_out_x: f32) -> Option<FadeEdge> {
    if pos.y > clip_rect.top() + FADE_HANDLE_ZONE_HEIGHT {
        return None;
    }
    if (pos.x - fade_in_x).abs() <= FADE_HANDLE_HIT_RADIUS {
        return Some(FadeEdge::In);
    }
    if (pos.x - fade_out_x).abs() <= FADE_HANDLE_HIT_RADIUS {
        return Some(FadeEdge::Out);
    }
    None
}

/// Comincia a trascinare l'estremità (durata) di una transizione già
/// presente: come `begin_fade_drag`, sempre locale alla singola clip.
fn begin_transition_drag(state: &mut TimelineState, visual: &ClipVisual, edge: FadeEdge) {
    let original_value = match edge {
        FadeEdge::In => visual.clip.effects.transition_in.as_ref(),
        FadeEdge::Out => visual.clip.effects.transition_out.as_ref(),
    }
    .map_or(0, |t| t.duration);
    state.transition_drag = Some(TransitionDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
    });
}

/// La clip adiacente a `clip_id` dal lato `edge`, sulla stessa track: `In`
/// cerca chi tocca il suo inizio, `Out` chi tocca la sua fine. `None` se
/// `clip_id` non esiste o non ha vicini da quel lato.
fn adjacent_clip(track: &vv_core::Track, clip_id: ClipId, edge: FadeEdge) -> Option<ClipId> {
    let clip = track.clip(clip_id)?;
    let at = match edge {
        FadeEdge::In => clip.timeline_start,
        FadeEdge::Out => clip.timeline_end(),
    };
    track
        .clips
        .iter()
        .find(|c| {
            c.id != clip_id
                && match edge {
                    FadeEdge::In => c.timeline_end() == at,
                    FadeEdge::Out => c.timeline_start == at,
                }
        })
        .map(|c| c.id)
}

/// Crea (o sostituisce) la crossing transition tra `left_id` e `right_id`,
/// clampando `transition.duration` a quanto le due clip possono davvero
/// "prestarle" (il doppio della più corta delle due, vedi
/// `CrossTransition::split`), e la seleziona.
fn apply_new_crossing(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    track_index: usize,
    left_id: ClipId,
    right_id: ClipId,
    mut transition: vv_core::Transition,
) {
    let track = &project.timelines[timeline_id].tracks[track_index];
    let (Some(left), Some(right)) = (track.clip(left_id), track.clip(right_id)) else {
        return;
    };
    let max_duration = (2 * left.timeline_len.min(right.timeline_len)).max(1);
    transition.duration = transition.duration.clamp(1, max_duration);
    let crossing = vv_core::CrossTransition { left_clip: left_id, right_clip: right_id, transition };
    history.do_command(
        project,
        Box::new(vv_core::SetCrossTransition::new(timeline_id, track_index, left_id, Some(crossing))),
    );
    state.selected.clear();
    state.selection_anchor = None;
    state.selected_gap = None;
    state.selected_transition = Some(TransitionSelection::Crossing(track_index, left_id));
}

/// Comincia un drag simmetrico della durata di una crossing transition:
/// `edge` è il bordo di *questa* clip da cui parte il drag — `Out` vuol
/// dire che questa clip è il `left_clip` della crossing (l'estremità presa
/// è quella dentro di lei), `In` che è il `right_clip` — coerente con
/// `Track::crossing_from`/`crossing_into` usati da `edge_marker`.
fn begin_crossing_drag(
    state: &mut TimelineState,
    project: &Project,
    timeline_id: TimelineId,
    track_index: usize,
    left_clip: ClipId,
    edge: FadeEdge,
) {
    let track = &project.timelines[timeline_id].tracks[track_index];
    let Some(crossing) = track.crossing_from(left_clip) else {
        return;
    };
    let (Some(left), Some(right)) = (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) else {
        return;
    };
    let max_duration = (2 * left.timeline_len.min(right.timeline_len)).max(1);
    state.crossing_drag = Some(CrossingDragState {
        track_index,
        left_clip,
        grabbed_left_side: edge == FadeEdge::Out,
        original_duration: crossing.transition.duration,
        max_duration,
        accum_px: 0.0,
    });
}

/// Comincia un Alt+drag di duplicazione dal corpo di un marker di
/// transizione (bordo singolo o crossing): il payload DnD va impostato
/// proprio qui, non in `resp.dragged()` come si potrebbe pensare per
/// analogia col resto del file — `Response::dnd_set_drag_payload` agisce
/// solo se `drag_started()`, non ad ogni frame di drag (la libreria lo
/// tiene poi vivo da sé finché dura il drag, vedi `egui::DragAndDrop`).
fn begin_transition_duplicate_drag(
    state: &mut TimelineState,
    resp: &egui::Response,
    project: &Project,
    timeline_id: TimelineId,
    visual: &ClipVisual,
    marker: &EdgeMarker,
) {
    let transition = match marker.selection {
        TransitionSelection::Edge(_, edge) => match edge {
            FadeEdge::In => visual.clip.effects.transition_in.clone(),
            FadeEdge::Out => visual.clip.effects.transition_out.clone(),
        },
        TransitionSelection::Crossing(track_index, left_clip) => project.timelines[timeline_id]
            .tracks[track_index]
            .crossing_from(left_clip)
            .map(|c| c.transition.clone()),
    };
    if let Some(transition) = transition {
        resp.dnd_set_drag_payload(transition);
    }
    state.transition_duplicate_drag = Some(visual.clip.id);
}

/// Valore (in frame, clampato a 1..=durata della clip) dell'anteprima di un
/// drag di transizione in corso: stessa convenzione di segno di
/// `fade_drag_value` (trascinare l'estremità verso l'interno della clip
/// allunga la transizione, in entrambi i bordi).
fn transition_drag_value(d: &TransitionDragState, clip_len: FrameIdx, px_per_frame: f32) -> FrameIdx {
    let signed_delta = match d.edge {
        FadeEdge::In => d.accum_px,
        FadeEdge::Out => -d.accum_px,
    };
    (d.original_value as f32 + signed_delta / px_per_frame)
        .round()
        .clamp(1.0, clip_len.max(1) as f32) as FrameIdx
}

/// Durata totale (in frame) dell'anteprima di un drag di crossing in corso:
/// simmetrico, trascinare l'estremità di sinistra verso sinistra allunga
/// (e quella di destra verso destra allo stesso modo), sempre di due volte
/// lo spostamento in frame — cresce/si accorcia sui due lati alla pari.
fn crossing_drag_value(d: &CrossingDragState, px_per_frame: f32) -> FrameIdx {
    // Come `transition_drag_value`: l'estremità sinistra è un bordo "Out"
    // (dentro la clip di sinistra, cresce trascinandola verso sinistra,
    // lontano dal taglio), quella destra un bordo "In" (dentro la clip di
    // destra, cresce trascinandola verso destra) — qui in più raddoppiato
    // sull'altro lato, vedi sopra.
    let signed_delta = if d.grabbed_left_side { -d.accum_px } else { d.accum_px };
    (d.original_duration as f32 + 2.0 * signed_delta / px_per_frame)
        .round()
        .clamp(1.0, d.max_duration.max(1) as f32) as FrameIdx
}

/// Cosa mostrare/selezionare sul bordo `edge` di `visual.clip`: un bordo
/// singolo (`EffectStack::transition_in`/`_out`), o — se quel bordo è
/// condiviso con una crossing transition valida — la propria metà di
/// quella. La durata riflette l'anteprima di un drag di ridimensionamento
/// in corso su questo bordo, se c'è.
struct EdgeMarker {
    duration: FrameIdx,
    selection: TransitionSelection,
    is_crossing: bool,
}

fn edge_marker(
    track: &vv_core::Track,
    state: &TimelineState,
    visual: &ClipVisual,
    px_per_frame: f32,
    edge: FadeEdge,
) -> Option<EdgeMarker> {
    let crossing = match edge {
        FadeEdge::In => track.crossing_into(visual.clip.id),
        FadeEdge::Out => track.crossing_from(visual.clip.id),
    };
    if let Some(crossing) = crossing {
        let total = if let Some(d) = &state.crossing_drag
            && d.track_index == visual.track_index
            && d.left_clip == crossing.left_clip
        {
            crossing_drag_value(d, px_per_frame)
        } else {
            crossing.transition.duration
        };
        let (split_left, split_right) = vv_core::CrossTransition::split_duration(total);
        let duration = match edge {
            FadeEdge::Out => split_left,
            FadeEdge::In => split_right,
        };
        return Some(EdgeMarker {
            duration,
            selection: TransitionSelection::Crossing(visual.track_index, crossing.left_clip),
            is_crossing: true,
        });
    }
    let transition = match edge {
        FadeEdge::In => visual.clip.effects.transition_in.as_ref(),
        FadeEdge::Out => visual.clip.effects.transition_out.as_ref(),
    }?;
    let mut duration = transition.duration;
    if let Some(d) = &state.transition_drag
        && d.clip_id == visual.clip.id
        && d.edge == edge
    {
        duration = transition_drag_value(d, visual.clip.timeline_len, px_per_frame);
    }
    Some(EdgeMarker {
        duration,
        selection: TransitionSelection::Edge((visual.track_index, visual.clip.id), edge),
        is_crossing: false,
    })
}

/// Estremità (durata) di una transizione sotto `pos`, solo nella banda in
/// basso alla clip — speculare a `fade_zone_at`. `None` per un bordo che
/// non ha ancora una transizione: niente da trascinare lì.
fn transition_handle_at(
    pos: egui::Pos2,
    clip_rect: egui::Rect,
    in_x: Option<f32>,
    out_x: Option<f32>,
) -> Option<FadeEdge> {
    if pos.y < clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT {
        return None;
    }
    if let Some(x) = in_x
        && (pos.x - x).abs() <= TRANSITION_HANDLE_HIT_RADIUS
    {
        return Some(FadeEdge::In);
    }
    if let Some(x) = out_x
        && (pos.x - x).abs() <= TRANSITION_HANDLE_HIT_RADIUS
    {
        return Some(FadeEdge::Out);
    }
    None
}

/// `true` se `pos` cade nel corpo del marker di una transizione (dal bordo
/// della clip alla sua estremità `x`), non solo sulla sua maniglia: un
/// click ovunque lì la seleziona, non serve mirare all'estremità.
fn transition_body_hit(pos: egui::Pos2, clip_rect: egui::Rect, edge: FadeEdge, x: f32) -> bool {
    if pos.y < clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT {
        return false;
    }
    match edge {
        FadeEdge::In => pos.x >= clip_rect.left() && pos.x <= x,
        FadeEdge::Out => pos.x <= clip_rect.right() && pos.x >= x,
    }
}

/// Bordo di `clip_rect` più vicino a `pos`, entro `drop_zone_px` — condiviso
/// dal drop di una `TransitionKind` dal pannello Effects e dal drop di una
/// `Transition` intera (duplicazione via Alt+drag): stessa regola "solo
/// vicino a un bordo" in entrambi i casi.
fn transition_drop_edge(pos: egui::Pos2, clip_rect: egui::Rect, drop_zone_px: f32) -> Option<FadeEdge> {
    if pos.x - clip_rect.left() <= drop_zone_px {
        Some(FadeEdge::In)
    } else if clip_rect.right() - pos.x <= drop_zone_px {
        Some(FadeEdge::Out)
    } else {
        None
    }
}

/// La `ClipVisual` adiacente a `clip_id` dal lato `edge`, sulla stessa
/// track — `None` se non ce n'è una. Usata durante il drag di una
/// transizione per anticipare, col colore del marker, se il rilascio lì
/// creerà una crossing o resterà un bordo singolo (vedi `CROSSING_COLOR`),
/// e per disegnare anche sulla clip vicina il marker speculare (vedi
/// `paint_mirrored_marker_on_neighbor`).
fn neighbor_visual<'a, 'b>(
    visuals: &'a [ClipVisual<'b>],
    track_index: usize,
    clip_id: ClipId,
    edge: FadeEdge,
) -> Option<&'a ClipVisual<'b>> {
    let visual = visuals.iter().find(|v| v.track_index == track_index && v.clip.id == clip_id)?;
    let at = match edge {
        FadeEdge::In => visual.clip.timeline_start,
        FadeEdge::Out => visual.clip.timeline_end(),
    };
    visuals.iter().find(|v| {
        v.track_index == track_index
            && v.clip.id != clip_id
            && match edge {
                FadeEdge::In => v.clip.timeline_end() == at,
                FadeEdge::Out => v.clip.timeline_start == at,
            }
    })
}

fn has_neighbor(visuals: &[ClipVisual], track_index: usize, clip_id: ClipId, edge: FadeEdge) -> bool {
    neighbor_visual(visuals, track_index, clip_id, edge).is_some()
}

/// Se c'è una clip adiacente dal lato `edge`, disegna anche su di lei il
/// marker speculare (bordo opposto): da rilasciata, una crossing si vede
/// già così su entrambe le clip (vedi doc di `paint_transition_marker`) —
/// mostrarla solo sulla clip sotto il puntatore durante il drag sarebbe
/// fuorviante. Va chiamata DOPO il loop che disegna tutte le clip (vedi
/// `pending_crossing_previews`): il vicino può venire dopo in `draw_order`,
/// e il suo stesso `paint_clip_box` la ricoprirebbe se disegnata durante
/// l'iterazione della clip sotto il puntatore. La clip vicina non è mai in
/// drag/trim quando questa funzione viene chiamata (un solo drag alla
/// volta, e questo è il drag di un `TransitionKind`/`Transition` dal
/// pannello Effects), quindi il suo rettangolo statico basta.
fn paint_mirrored_marker_on_neighbor(
    painter: &egui::Painter,
    visuals: &[ClipVisual],
    origin: egui::Pos2,
    row_y: &[f32],
    px_per_frame: f32,
    track_index: usize,
    clip_id: ClipId,
    edge: FadeEdge,
) {
    let Some(neighbor) = neighbor_visual(visuals, track_index, clip_id, edge) else {
        return;
    };
    let x = origin.x + neighbor.clip.timeline_start as f32 * px_per_frame;
    let y = origin.y + row_y[track_index];
    let w = (neighbor.clip.timeline_len as f32 * px_per_frame).max(2.0);
    let neighbor_rect = egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0));
    let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(neighbor_rect.width() / 2.0);
    let opposite = match edge {
        FadeEdge::In => FadeEdge::Out,
        FadeEdge::Out => FadeEdge::In,
    };
    let nx = match opposite {
        FadeEdge::In => neighbor_rect.left() + drop_zone_px,
        FadeEdge::Out => neighbor_rect.right() - drop_zone_px,
    };
    paint_transition_marker(painter, neighbor_rect, opposite, nx, false, true);
}

/// Il marker di una transizione: una fascia colorata in basso alla clip dal
/// bordo a `x`. Un bordo singolo (`is_crossing: false`) mostra un
/// ingranaggio sul lato fisso (il bordo vero della clip, contro il
/// trasparente) e una parentesi sull'estremità `x` (il lato trascinabile) —
/// "X]" per `In`, "[X" per `Out". Una crossing (`is_crossing: true`) non ha
/// un lato "fisso": il bordo condiviso col vicino mostra un'altra
/// parentesi, aperta verso l'interno della propria metà — le due metà,
/// disegnate una per clip, si affiancano lì in "][".
fn paint_transition_marker(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    edge: FadeEdge,
    x: f32,
    selected: bool,
    is_crossing: bool,
) {
    let x = x.clamp(clip_rect.left(), clip_rect.right());
    let band = egui::Rect::from_min_max(
        egui::pos2(clip_rect.left(), clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT),
        clip_rect.max,
    );
    let body = match edge {
        FadeEdge::In => egui::Rect::from_min_max(band.min, egui::pos2(x, band.bottom())),
        FadeEdge::Out => egui::Rect::from_min_max(egui::pos2(x, band.top()), band.max),
    };
    let color = match (is_crossing, selected) {
        (false, false) => TRANSITION_COLOR,
        (false, true) => TRANSITION_SELECTED_COLOR,
        (true, false) => CROSSING_COLOR,
        (true, true) => CROSSING_SELECTED_COLOR,
    };
    painter.rect_filled(body, 0.0, color);
    let edge_x = match edge {
        FadeEdge::In => clip_rect.left(),
        FadeEdge::Out => clip_rect.right(),
    };
    if is_crossing {
        let opposite = match edge {
            FadeEdge::In => FadeEdge::Out,
            FadeEdge::Out => FadeEdge::In,
        };
        paint_bracket_icon(painter, egui::pos2(edge_x, band.center().y), band.height() * 0.7, opposite, egui::Color32::WHITE);
    } else {
        paint_gear_icon(painter, egui::pos2(edge_x, band.center().y), band.height() * 0.4, egui::Color32::WHITE);
    }
    paint_bracket_icon(painter, egui::pos2(x, band.center().y), band.height() * 0.7, edge, egui::Color32::WHITE);
}

/// Parentesi disegnata a mano (nessun glifo Unicode, vedi `paint_gear_icon`):
/// una linea verticale con due tacche che aprono verso il bordo fisso della
/// clip (`In`: tacche a sinistra, verso l'ingranaggio; `Out`: a destra).
pub(crate) fn paint_bracket_icon(painter: &egui::Painter, center: egui::Pos2, height: f32, edge: FadeEdge, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.6, color);
    let half = height / 2.0;
    let tick = height * 0.3;
    let tick_dir = match edge {
        FadeEdge::In => -1.0,
        FadeEdge::Out => 1.0,
    };
    painter.line_segment(
        [egui::pos2(center.x, center.y - half), egui::pos2(center.x, center.y + half)],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y - half),
            egui::pos2(center.x + tick * tick_dir, center.y - half),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y + half),
            egui::pos2(center.x + tick * tick_dir, center.y + half),
        ],
        stroke,
    );
}

/// Transizioni del pannello Effects, in ordine di comparsa lì — vedi
/// `ALL_FILTER_KINDS`, stessa idea.
pub const ALL_TRANSITION_KINDS: [vv_core::TransitionKind; 1] = [vv_core::TransitionKind::Push];

pub fn transition_kind_label(kind: vv_core::TransitionKind) -> std::borrow::Cow<'static, str> {
    match kind {
        vv_core::TransitionKind::Push => t!("transition.push"),
    }
}

pub fn push_direction_label(direction: vv_core::PushDirection) -> std::borrow::Cow<'static, str> {
    match direction {
        vv_core::PushDirection::Left => t!("transition.direction_left"),
        vv_core::PushDirection::Right => t!("transition.direction_right"),
        vv_core::PushDirection::Up => t!("transition.direction_up"),
        vv_core::PushDirection::Down => t!("transition.direction_down"),
    }
}

pub fn ease_label(ease: vv_core::Ease) -> std::borrow::Cow<'static, str> {
    match ease {
        vv_core::Ease::None => t!("transition.ease_none"),
        vv_core::Ease::In => t!("transition.ease_in"),
        vv_core::Ease::Out => t!("transition.ease_out"),
        vv_core::Ease::InOut => t!("transition.ease_in_out"),
    }
}

/// Offset verticale normalizzato (-1 in basso, +1 in alto) della riga del
/// volume per un gain in dB: centrato a 0 dB, i due rami usano scale diverse
/// perché `GAIN_DB_MIN`/`GAIN_DB_MAX` non sono simmetrici.
fn gain_offset(db: f32) -> f32 {
    if db >= 0.0 {
        (db / vv_core::GAIN_DB_MAX).clamp(0.0, 1.0)
    } else {
        -(db / vv_core::GAIN_DB_MIN).clamp(0.0, 1.0)
    }
}

/// Inversa di `gain_offset`.
fn gain_from_offset(offset: f32) -> f32 {
    let offset = offset.clamp(-1.0, 1.0);
    if offset >= 0.0 {
        offset * vv_core::GAIN_DB_MAX
    } else {
        -offset * vv_core::GAIN_DB_MIN
    }
}

/// Coordinata Y della riga del volume per un gain in dB.
fn gain_line_y(db: f32, clip_rect: egui::Rect) -> f32 {
    clip_rect.center().y - gain_offset(db) * clip_rect.height() / 2.0
}

fn paint_gain_line(painter: &egui::Painter, clip_rect: egui::Rect, line_y: f32, dragging: bool) {
    let alpha = if dragging { 220 } else { 130 };
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha));
    painter.line_segment(
        [egui::pos2(clip_rect.left(), line_y), egui::pos2(clip_rect.right(), line_y)],
        stroke,
    );
}

/// La riga del volume è larga quanto la clip ma sottile: si afferra entro
/// `VOLUME_LINE_HIT_PX` in verticale, non serve un test orizzontale stretto.
fn volume_line_hit(pos: egui::Pos2, clip_rect: egui::Rect, line_y: f32) -> bool {
    clip_rect.x_range().contains(pos.x) && (pos.y - line_y).abs() <= VOLUME_LINE_HIT_PX
}

/// Comincia a trascinare la riga del volume: come il fade, locale alla
/// singola clip. Apre il gruppo di undo che raccoglierà i `SetGain` di
/// ogni frame del drag (vedi `VolumeDragState::group`).
fn begin_volume_drag(state: &mut TimelineState, history: &mut History, visual: &ClipVisual) {
    state.volume_drag = Some(VolumeDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        original_db: visual.clip.effects.gain_db.default,
        accum_px: 0.0,
        group: history.begin_group(),
    });
}

/// Gain (dB) dell'anteprima di un drag della riga volume in corso: il drag è
/// verticale e lineare nello spazio "offset" disegnato, non in dB, così la
/// riga segue esattamente il puntatore lungo tutta la corsa.
fn volume_drag_value(d: &VolumeDragState, half_height: f32) -> f32 {
    if half_height <= 0.0 {
        return d.original_db;
    }
    let offset = gain_offset(d.original_db) - d.accum_px / half_height;
    gain_from_offset(offset)
}

/// Trim da applicare al rilascio: lo stesso valore (già clampato) mostrato
/// nell'anteprima.
fn finish_trim(
    t: &TrimState,
    visual: &ClipVisual,
    visuals: &[ClipVisual],
    trimmed_primary_new_value: Option<FrameIdx>,
) -> PendingAction {
    let new_value = trimmed_primary_new_value.unwrap_or(t.original_value);
    let mut trims = vec![(t.clip_id, t.track_index, t.edge, new_value)];
    let mut overwritten: Vec<_> =
        grown_range(&visual.clip, visual.track_index, t.edge, new_value).into_iter().collect();
    for &(other_id, other_track, offset, edge) in &t.followers {
        let Some(other) =
            visuals.iter().find(|v| v.clip.id == other_id && v.track_index == other_track)
        else {
            continue;
        };
        let other_value = new_value + offset;
        trims.push((other_id, other_track, edge, other_value));
        overwritten.extend(grown_range(&other.clip, other_track, edge, other_value));
    }
    PendingAction::Trim { trims, overwritten }
}

/// Spostamento da applicare al rilascio, alla stessa posizione mostrata
/// durante il drag.
fn finish_drag(
    d: &DragState,
    targets: &[(ClipId, EffectiveTrack)],
    track_kinds: &[TrackKind],
    dragged_primary_new_start: Option<FrameIdx>,
) -> PendingAction {
    let new_start = dragged_primary_new_start.unwrap_or(d.original_start);
    let original_tracks =
        std::iter::once(d.track_index).chain(d.followers.iter().map(|(_, t, _)| *t));
    let starts = std::iter::once(new_start)
        .chain(d.followers.iter().map(|(_, _, offset)| new_start + offset));

    let mut new_video_tracks = 0usize;
    let mut new_audio_tracks = 0usize;
    let moves: Vec<(ClipId, usize, EffectiveTrack, FrameIdx)> = targets
        .iter()
        .zip(original_tracks)
        .zip(starts)
        .map(|(((id, target), from_track), start)| {
            if let EffectiveTrack::New(depth) = *target {
                let count = match track_kinds[from_track] {
                    TrackKind::Video => &mut new_video_tracks,
                    TrackKind::Audio => &mut new_audio_tracks,
                };
                *count = (*count).max(depth);
            }
            (*id, from_track, *target, start)
        })
        .collect();
    PendingAction::Move {
        new_video_tracks,
        new_audio_tracks,
        moves,
        duplicate: d.duplicate,
    }
}

/// Applica un passo di scroll cinetico a `scroll_val`: smorza `vel` (px/s)
/// con la stessa fisica ad attrito del drag-to-scroll nativo di egui, e
/// azzera la velocità se lo scroll risultante sbatte contro un limite.
/// Ritorna `true` se `scroll_val` è stato aggiornato (serve un repaint).
fn apply_kinetic_scroll(scroll_val: &mut f32, vel: &mut f32, max_scroll: f32, dt: f32) -> bool {
    if *vel == 0.0 {
        return false;
    }
    let friction = KINETIC_FRICTION * dt;
    if friction > vel.abs() || vel.abs() < KINETIC_STOP_SPEED {
        *vel = 0.0;
        return false;
    }
    *vel -= friction * vel.signum();
    let raw = *scroll_val + *vel * dt;
    let clamped = raw.clamp(0.0, max_scroll);
    if clamped != raw {
        *vel = 0.0;
    }
    *scroll_val = clamped;
    true
}

/// Corregge lo scroll salvato della ScrollArea `scroll_id` prima del suo `show`:
/// allo zoom la testina resta ferma a schermo, in riproduzione resta visibile;
/// gestisce anche lo scroll orizzontale cinetico da touchpad (swipe + inerzia
/// dopo il rilascio), dato che lo scroll a rotella verticale è già consumato
/// altrove per i riquadri Video/Audio.
fn sync_timeline_scroll(
    ctx: &egui::Context,
    scroll_id: egui::Id,
    state: &mut TimelineState,
    fps: f64,
    px_per_frame: f32,
    viewport_width: f32,
    content_width: f32,
    playback_active: bool,
    panel_rect: egui::Rect,
    kinetic_scroll_enabled: bool,
) {
    // Zoom cambiato in questo frame: si corregge lo scroll salvato della
    // ScrollArea (stesso id) prima del `show`, così la testina resta ferma a
    // schermo.
    if state.pixels_per_sec != state.last_rendered_pps {
        if let Some(mut scroll_state) =
            egui::containers::scroll_area::State::load(ctx, scroll_id)
        {
            let playhead_secs = state.playhead as f64 / fps;
            scroll_state.offset.x +=
                (playhead_secs as f32) * (state.pixels_per_sec - state.last_rendered_pps);
            scroll_state.store(ctx, scroll_id);
        }
    }
    state.last_rendered_pps = state.pixels_per_sec;

    // In riproduzione la testina resta visibile: se esce si "volta pagina"
    // portandola a un terzo da sinistra. Clamp come quello di egui in `begin`.
    if playback_active {
        let playhead_x = state.playhead as f32 * px_per_frame;
        let visible_start =
            match egui::containers::scroll_area::State::load(ctx, scroll_id) {
                Some(st) => st.offset.x,
                None => 0.0,
            };
        let visible_end = visible_start + viewport_width;
        const FOLLOW_MARGIN_FRAC: f32 = 1.0 / 3.0;
        if playhead_x < visible_start || playhead_x > visible_end {
            let target = (playhead_x - viewport_width * FOLLOW_MARGIN_FRAC)
                .clamp(0.0, (content_width - viewport_width).max(0.0));
            if let Some(mut scroll_state) =
                egui::containers::scroll_area::State::load(ctx, scroll_id)
            {
                scroll_state.offset.x = target;
                scroll_state.store(ctx, scroll_id);
            }
        }
    }

    let max_offset_x = (content_width - viewport_width).max(0.0);
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    // In riproduzione la vista segue la testina: un'inerzia residua la
    // farebbe scivolare via dal punto in cui l'ha appena centrata sopra.
    if !kinetic_scroll_enabled || playback_active {
        state.hscroll_vel = 0.0;
    }
    let hovering_panel = ctx
        .input(|i| i.pointer.hover_pos())
        .is_some_and(|p| panel_rect.contains(p));
    // Swipe orizzontale in corso: applicato subito (come farebbe la
    // ScrollArea), e la sua velocità istantanea diventa l'inerzia da
    // smorzare quando il gesto finisce.
    let wheel_x = if hovering_panel {
        ctx.input(|i| i.smooth_scroll_delta.x)
    } else {
        0.0
    };
    if wheel_x != 0.0 {
        if let Some(mut scroll_state) = egui::containers::scroll_area::State::load(ctx, scroll_id)
        {
            scroll_state.offset.x = (scroll_state.offset.x - wheel_x).clamp(0.0, max_offset_x);
            scroll_state.store(ctx, scroll_id);
        }
        state.hscroll_vel = if kinetic_scroll_enabled && !playback_active && dt > 0.0 {
            -KINETIC_VELOCITY_GAIN * wheel_x / dt
        } else {
            0.0
        };
        // Consumata qui: la ScrollArea non deve riapplicarla nel suo `show`.
        ctx.input_mut(|i| i.smooth_scroll_delta.x = 0.0);
    } else if let Some(mut scroll_state) = egui::containers::scroll_area::State::load(ctx, scroll_id)
    {
        let mut offset_x = scroll_state.offset.x;
        if apply_kinetic_scroll(&mut offset_x, &mut state.hscroll_vel, max_offset_x, dt) {
            scroll_state.offset.x = offset_x;
            scroll_state.store(ctx, scroll_id);
            ctx.request_repaint();
        }
    }
}

/// Righello: tacche, striscia dei frame in cache e marker di export; click e
/// drag spostano il playhead.
fn show_ruler(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    origin: egui::Pos2,
    content_width: f32,
    state: &mut TimelineState,
    visuals: &[ClipVisual],
    fps: f64,
    px_per_frame: f32,
    snapping_enabled: bool,
    buffered_ranges: &[(FrameIdx, FrameIdx)],
    max_end_frames: FrameIdx,
) {
    let ruler_rect =
        egui::Rect::from_min_size(origin, egui::vec2(content_width, RULER_HEIGHT));
    painter.rect_filled(ruler_rect, 0.0, egui::Color32::from_gray(45));
    let ruler_resp = ui.interact(
        ruler_rect,
        ui.id().with("timeline_ruler"),
        egui::Sense::click_and_drag(),
    );
    // Su un click conta dove è stato rilasciato: se il frame è
    // arrivato in ritardo, `interact_pointer_pos` è già
    // l'ultima posizione del mouse dopo il rilascio.
    let ruler_pos = if ruler_resp.clicked() {
        ui.input(|i| {
            i.events.iter().rev().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                } => Some(*pos),
                _ => None,
            })
        })
        .or_else(|| ruler_resp.interact_pointer_pos())
    } else {
        ruler_resp.interact_pointer_pos()
    };
    if let Some(pos) = ruler_pos {
        let raw_frame =
            (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
        state.playhead =
            snap_frame(raw_frame, 0, &visuals, &[], &[], px_per_frame, snapping_enabled);
    }

    // Solo le tacche visibili: una timeline lunga zoomata al frame ne avrebbe
    // migliaia fuori schermo.
    let visible_x = ui.clip_rect().intersect(ruler_rect);
    draw_ruler_ticks(&painter, origin, visible_x, state.pixels_per_sec, fps);

    // Sotto la linea della playhead, così resta visibile.
    const BUFFERED_STRIP_HEIGHT: f32 = 4.0;
    let buffered_color = egui::Color32::from_rgba_unmultiplied(120, 190, 255, 140);
    for &(start, end) in buffered_ranges {
        let x0 = origin.x + start as f32 * px_per_frame;
        let x1 = origin.x + (end + 1) as f32 * px_per_frame;
        let strip_rect = egui::Rect::from_min_max(
            egui::pos2(x0, origin.y + RULER_HEIGHT - BUFFERED_STRIP_HEIGHT),
            egui::pos2(x1, origin.y + RULER_HEIGHT),
        );
        painter.rect_filled(strip_rect, 0.0, buffered_color);
    }

    if !state.export_marks.is_full(max_end_frames) {
        let (mark_in, mark_out) = state.export_marks.resolve(max_end_frames);
        let band = egui::Rect::from_min_max(
            egui::pos2(origin.x + mark_in as f32 * px_per_frame, origin.y),
            egui::pos2(
                origin.x + mark_out as f32 * px_per_frame,
                origin.y + RULER_HEIGHT - BUFFERED_STRIP_HEIGHT,
            ),
        );
        painter.rect_filled(
            band,
            0.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
        );
        for x in [band.left(), band.right()] {
            painter.vline(
                x,
                band.y_range(),
                egui::Stroke::new(1.0, egui::Color32::from_gray(185)),
            );
        }
    }
}

/// Linea del playhead più una testina triangolare nel righello.
fn paint_playhead(painter: &egui::Painter, origin: egui::Pos2, x_offset: f32, visual_height: f32) {
    let px = origin.x + x_offset;
    let playhead_color = egui::Color32::from_rgb(220, 50, 50);
    painter.line_segment(
        [
            egui::pos2(px, origin.y),
            egui::pos2(px, origin.y + visual_height),
        ],
        egui::Stroke::new(2.0, playhead_color),
    );
    const PLAYHEAD_HEAD_HALF_WIDTH: f32 = 6.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(px - PLAYHEAD_HEAD_HALF_WIDTH, origin.y),
            egui::pos2(px + PLAYHEAD_HEAD_HALF_WIDTH, origin.y),
            egui::pos2(px, origin.y + RULER_HEIGHT),
        ],
        playhead_color,
        egui::Stroke::NONE,
    ));
}

fn apply_pending_action(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    action: PendingAction,
) {
    match action {
        PendingAction::Move {
            new_video_tracks,
            new_audio_tracks,
            moves,
            duplicate,
        } => {
            // Creare in ordine di depth crescente basta: sia per
            // video sia per audio, la depth-esima creata finisce da
            // sé alla riga giusta (vedi `EffectiveTrack::New`).
            let mut video_tracks = Vec::with_capacity(new_video_tracks);
            for _ in 0..new_video_tracks {
                video_tracks.push(add_track(project, history, timeline_id, TrackKind::Video));
            }
            let mut audio_tracks = Vec::with_capacity(new_audio_tracks);
            for _ in 0..new_audio_tracks {
                audio_tracks.push(add_track(project, history, timeline_id, TrackKind::Audio));
            }
            let moves: Vec<(ClipId, usize, usize, FrameIdx)> = moves
                .into_iter()
                .map(|(id, from_track, dest, start)| {
                    let to_track = match dest {
                        EffectiveTrack::Existing(track) => track,
                        // Il tipo della track nuova è quello di partenza.
                        EffectiveTrack::New(depth) => {
                            match project.timelines[timeline_id].tracks[from_track].kind {
                                TrackKind::Video => video_tracks[depth - 1],
                                TrackKind::Audio => audio_tracks[depth - 1],
                            }
                        }
                    };
                    (id, from_track, to_track, start)
                })
                .collect();
            if duplicate {
                duplicate_clips(project, history, state, timeline_id, &moves);
                return;
            }
            // Le destinazioni sovrascrivono quel che c'era; le clip spostate restano
            // fuori, anche nella posizione di partenza.
            let ranges: Vec<(usize, FrameIdx, FrameIdx)> = moves
                .iter()
                .filter_map(|&(id, from_track, to_track, start)| {
                    let len = project.timelines[timeline_id]
                        .clip(from_track, id)?
                        .timeline_len;
                    Some((to_track, start, start + len))
                })
                .collect();
            let exclude: Vec<(usize, ClipId)> = moves
                .iter()
                .flat_map(|&(id, from_track, to_track, _)| [(from_track, id), (to_track, id)])
                .collect();
            let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
            vv_core::make_room_for_ranges(
                project,
                timeline_id,
                &ranges,
                &exclude,
                &mut commands,
            );
            commands.push(Box::new(vv_core::MoveClips::new(timeline_id, moves)));
            history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::MoveClips, commands)));
        }
        PendingAction::Trim { trims, overwritten } => {
            // Il tratto guadagnato allungando sovrascrive quel che c'era; le clip
            // trimmate restano fuori, o si taglierebbero da sole.
            let exclude: Vec<(usize, ClipId)> = trims
                .iter()
                .map(|&(clip_id, track_index, _, _)| (track_index, clip_id))
                .collect();
            let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
            vv_core::make_room_for_ranges(
                project,
                timeline_id,
                &overwritten,
                &exclude,
                &mut commands,
            );
            commands.extend(trims.into_iter().map(
                |(clip_id, track_index, edge, new_value)| {
                    Box::new(vv_core::TrimClip::new(
                        timeline_id,
                        track_index,
                        clip_id,
                        edge,
                        new_value,
                    )) as Box<dyn vv_core::Command>
                },
            ));
            history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::TrimClips, commands)));
        }
        PendingAction::SetFade { track_index, clip_id, edge, new_value } => {
            history.do_command(
                project,
                Box::new(vv_core::SetClipFade::new(timeline_id, track_index, clip_id, edge, new_value)),
            );
        }
        PendingAction::SetGain { track_index, clip_id, new_value } => {
            history.do_command(
                project,
                Box::new(vv_core::set_clip_gain(timeline_id, track_index, clip_id, new_value)),
            );
        }
        PendingAction::ApplyFilter { track_index, clip_id, filter } => {
            // Se è già presente (ridropparlo) lo si riattiva soltanto,
            // invece di duplicarlo in coda.
            let new_filters = project.timelines[timeline_id].clip(track_index, clip_id).map(|clip| {
                let mut filters = clip.effects.filters.clone();
                match filters.iter_mut().find(|f| f.kind == filter) {
                    Some(existing) => existing.enabled = true,
                    None => filters.push(vv_core::ClipFilter { kind: filter, enabled: true }),
                }
                filters
            });
            if let Some(filters) = new_filters {
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_filters(timeline_id, track_index, clip_id, filters)),
                );
            }
        }
        PendingAction::ApplyTransition { track_index, clip_id, edge, kind } => {
            let default_duration =
                (project.timelines[timeline_id].fps.as_f64() * 0.45).round() as FrameIdx;
            let transition = vv_core::Transition {
                kind,
                duration: default_duration.max(1),
                direction: vv_core::PushDirection::Right,
                ease: vv_core::Ease::InOut,
                curve: 0.5,
            };
            let neighbor = adjacent_clip(&project.timelines[timeline_id].tracks[track_index], clip_id, edge);
            if let Some(neighbor_id) = neighbor {
                let (left_id, right_id) = match edge {
                    FadeEdge::In => (neighbor_id, clip_id),
                    FadeEdge::Out => (clip_id, neighbor_id),
                };
                apply_new_crossing(project, history, state, timeline_id, track_index, left_id, right_id, transition);
            } else if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = transition;
                transition.duration = transition.duration.clamp(1, clip.timeline_len.max(1));
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                );
                state.selected.clear();
                state.selection_anchor = None;
                state.selected_gap = None;
                state.selected_transition = Some(TransitionSelection::Edge((track_index, clip_id), edge));
            }
        }
        PendingAction::SetTransitionDuration { track_index, clip_id, edge, new_value } => {
            if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = match edge {
                    FadeEdge::In => clip.effects.transition_in.clone(),
                    FadeEdge::Out => clip.effects.transition_out.clone(),
                };
                if let Some(t) = &mut transition {
                    t.duration = new_value.clamp(1, clip.timeline_len.max(1));
                }
                if let Some(transition) = transition {
                    history.do_command(
                        project,
                        Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                    );
                }
            }
        }
        PendingAction::SetCrossingDuration { track_index, left_clip, new_value } => {
            let track = &project.timelines[timeline_id].tracks[track_index];
            if let Some(mut crossing) = track.crossing_from(left_clip).cloned() {
                let max_duration = match (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) {
                    (Some(left), Some(right)) => (2 * left.timeline_len.min(right.timeline_len)).max(1),
                    _ => new_value.max(1),
                };
                crossing.transition.duration = new_value.clamp(1, max_duration);
                history.do_command(
                    project,
                    Box::new(vv_core::SetCrossTransition::new(timeline_id, track_index, left_clip, Some(crossing))),
                );
            }
        }
        PendingAction::DuplicateTransition { track_index, clip_id, edge, transition } => {
            let neighbor = adjacent_clip(&project.timelines[timeline_id].tracks[track_index], clip_id, edge);
            if let Some(neighbor_id) = neighbor {
                let (left_id, right_id) = match edge {
                    FadeEdge::In => (neighbor_id, clip_id),
                    FadeEdge::Out => (clip_id, neighbor_id),
                };
                apply_new_crossing(project, history, state, timeline_id, track_index, left_id, right_id, transition);
            } else if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = transition;
                transition.duration = transition.duration.clamp(1, clip.timeline_len.max(1));
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                );
                state.selected.clear();
                state.selection_anchor = None;
                state.selected_gap = None;
                state.selected_transition = Some(TransitionSelection::Edge((track_index, clip_id), edge));
            }
        }
        PendingAction::Unlink(track_index, clip_id) => {
            history.do_command(
                project,
                Box::new(vv_core::UnlinkClip::new(timeline_id, track_index, clip_id)),
            );
        }
        PendingAction::Link(targets) => {
            history.do_command(
                project,
                Box::new(vv_core::LinkClips::new(timeline_id, targets)),
            );
        }
        PendingAction::SetTrackFlag(track_index, flag, value) => {
            history.do_command(
                project,
                Box::new(vv_core::SetTrackFlag::new(timeline_id, track_index, flag, value)),
            );
            if flag == TrackFlag::Locked && value {
                state.drop_locked(&project.timelines[timeline_id]);
            }
        }
        PendingAction::RemoveTrack(track_index) => {
            history.do_command(
                project,
                Box::new(vv_core::RemoveTrack::new(timeline_id, track_index)),
            );
            // Gli indici di track della selezione non valgono più: si azzera.
            state.clear_selection();
        }
    }
}

/// Aggiunge una track in coda e ne restituisce l'indice.
pub fn add_track(
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    kind: TrackKind,
) -> usize {
    let index = project.timelines[timeline_id].tracks.len();
    history.do_command(project, Box::new(vv_core::AddTrack::new(timeline_id, kind)));
    index
}

/// Inserisce una copia di ogni clip di `moves` alla destinazione,
/// sovrascrivendo come uno spostamento. Le copie diventano la selezione.
fn duplicate_clips(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    moves: &[(ClipId, usize, usize, FrameIdx)],
) {
    let copies: Vec<(usize, Clip, Option<vv_core::LinkGroupId>)> = moves
        .iter()
        .filter_map(|&(id, from_track, to_track, start)| {
            let original = project.timelines[timeline_id].clip(from_track, id)?;
            let mut clip = original.clone();
            clip.timeline_start = start;
            clip.linked_group = None;
            Some((to_track, clip, original.linked_group))
        })
        .collect();
    let copies: Vec<_> = copies
        .into_iter()
        .map(|(track, mut clip, group)| {
            clip.id = project.alloc_clip_id();
            (track, clip, group)
        })
        .collect();
    let new_selection: BTreeSet<ClipKey> =
        copies.iter().map(|(track, clip, _)| (*track, clip.id)).collect();
    let commands = vv_core::insert_overwriting(project, timeline_id, copies);
    history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::DuplicateClips, commands)));

    let anchor = new_selection.iter().next().copied();
    state.set_selection(new_selection, anchor);
}

/// `offline`: il media della clip non è più nel media pool (cancellato da
/// lì, vedi `vv_core::RemoveMedia`) — la clip resta in timeline ma si tinge
/// di rosso, e il player mostra "Media offline".
fn clip_label_and_color(
    clip: &Clip,
    track: &Track,
    offline: bool,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
) -> (String, egui::Color32) {
    match &clip.source {
        vv_core::ClipSource::Media(media_id) => {
            if offline {
                return (t!("timeline.media_offline").into_owned(), OFFLINE_COLOR);
            }
            let label = media_labels(*media_id);
            let color = if track.kind == TrackKind::Video {
                egui::Color32::from_rgb(90, 140, 200)
            } else {
                egui::Color32::from_rgb(90, 190, 140)
            };
            (label, darken_if_edited(color, clip))
        }
        vv_core::ClipSource::SolidColor => (
            t!("generator.solid_color").into_owned(),
            darken_if_edited(egui::Color32::from_rgb(200, 170, 90), clip),
        ),
        vv_core::ClipSource::Text => (
            clip.effects
                .title
                .as_ref()
                .and_then(|t| t.content.lines().next())
                .map_or_else(|| t!("generator.text").into_owned(), str::to_string),
            darken_if_edited(egui::Color32::from_rgb(170, 110, 200), clip),
        ),
    }
}

/// Le clip con qualche effetto modificato rispetto al default si distinguono
/// a colpo d'occhio nella timeline: stesso colore, tonalità più scura.
fn darken_if_edited(color: egui::Color32, clip: &Clip) -> egui::Color32 {
    if clip.effects.is_pristine() {
        return color;
    }
    const F: f32 = 0.62;
    egui::Color32::from_rgb(
        (color.r() as f32 * F) as u8,
        (color.g() as f32 * F) as u8,
        (color.b() as f32 * F) as u8,
    )
}

const DISABLED_BADGE_SIZE: f32 = 12.0;

/// Quadratino rosso barrato davanti al nome di una clip disattivata.
fn paint_disabled_badge(painter: &egui::Painter, top_left: egui::Pos2) {
    let rect = egui::Rect::from_min_size(
        top_left + egui::vec2(0.0, 1.0),
        egui::vec2(DISABLED_BADGE_SIZE, DISABLED_BADGE_SIZE),
    );
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(200, 60, 60));
    let inner = rect.shrink(3.0);
    painter.line_segment(
        [inner.left_bottom(), inner.right_top()],
        egui::Stroke::new(1.5, egui::Color32::WHITE),
    );
}

/// Rettangolo di una clip in coordinate locali al contenuto: stessa
/// geometria per disegno, rettangolo di selezione e shift+click.
fn clip_local_rect(visual: &ClipVisual, px_per_frame: f32, row_y: &[f32]) -> egui::Rect {
    let x = visual.clip.timeline_start as f32 * px_per_frame;
    let y = row_y[visual.track_index];
    let w = (visual.clip.timeline_len as f32 * px_per_frame).max(2.0);
    egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0))
}

/// Waveform di una clip audio, una linea per colonna visibile. Il bin di
/// ogni colonna viene dal tempo assoluto nell'audio: dividere la clip non
/// sposta la forma d'onda.
fn draw_clip_waveform(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    peaks: &[f32],
    clip_start_secs: f64,
    clip_end_secs: f64,
    media_fps: f64,
    audio_duration_secs: f64,
    visible_rect: egui::Rect,
    gain_db: &Keyframed<f32>,
) {
    if peaks.is_empty() || audio_duration_secs <= 0.0 || media_fps <= 0.0 {
        return;
    }
    if clip_end_secs <= clip_start_secs {
        return;
    }

    // Porzione visibile della clip (nessuna colonna fuori dal viewport).
    let vis = clip_rect.intersect(visible_rect);
    if !vis.is_positive() {
        return;
    }

    let center_y = clip_rect.center().y;
    let half_height = clip_rect.height() / 2.0;
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 140));
    let clipped_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 90, 90, 200));

    let width = clip_rect.width();
    let mut x = vis.min.x;
    while x < vis.max.x {
        let frac = ((x - clip_rect.min.x) / width) as f64;
        let bin = waveform_bin_for_column(
            frac,
            clip_start_secs,
            clip_end_secs,
            audio_duration_secs,
            peaks.len(),
        );
        // Il gain vive in frame *sorgente*, come nel mixer: la forma
        // disegnata è quella che si sentirà davvero, clipping compreso.
        let secs = clip_start_secs + frac * (clip_end_secs - clip_start_secs);
        let source_frame = (secs * media_fps).floor() as FrameIdx;
        let amplified = peaks[bin] * vv_audio::mixer::db_to_linear(gain_db.value_at(source_frame));
        let h = (half_height * amplified.min(1.0)).max(0.5);
        painter.line_segment(
            [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
            if amplified > 1.0 { clipped_stroke } else { stroke },
        );
        x += 1.0;
    }
}

/// Bin della colonna a `frac` della clip, dal tempo assoluto nell'audio.
fn waveform_bin_for_column(
    frac: f64,
    clip_start_secs: f64,
    clip_end_secs: f64,
    audio_duration_secs: f64,
    num_peaks: usize,
) -> usize {
    let t_secs = clip_start_secs + frac * (clip_end_secs - clip_start_secs);
    ((t_secs / audio_duration_secs * num_peaks as f64) as usize).min(num_peaks.saturating_sub(1))
}

/// Le clip il cui rettangolo interseca `rect` (coordinate locali): nucleo
/// condiviso da marquee-select e shift+click (che usa il rettangolo che
/// unisce l'ancora e la clip cliccata).
fn clips_intersecting_rect(
    visuals: &[ClipVisual],
    px_per_frame: f32,
    row_y: &[f32],
    rect: egui::Rect,
) -> Vec<ClipKey> {
    visuals
        .iter()
        .filter(|v| !v.locked && clip_local_rect(v, px_per_frame, row_y).intersects(rect))
        .map(|v| (v.track_index, v.clip.id))
        .collect()
}

/// Il vuoto che copre `frame` sulla track, se seguito da un'altra clip.
fn gap_at(
    visuals: &[ClipVisual],
    track_index: usize,
    frame: FrameIdx,
) -> Option<(FrameIdx, FrameIdx)> {
    let mut track_clips: Vec<&Clip> = visuals
        .iter()
        .filter(|v| v.track_index == track_index)
        .map(|v| v.clip.as_ref())
        .collect();
    track_clips.sort_by_key(|c| c.timeline_start);

    if track_clips
        .iter()
        .any(|c| c.contains(frame))
    {
        return None; // `frame` è dentro a una clip, non in un vuoto.
    }
    let next = track_clips.iter().find(|c| c.timeline_start > frame)?;
    let gap_start = track_clips
        .iter()
        .filter(|c| c.timeline_end() <= frame)
        .map(|c| c.timeline_end())
        .max()
        .unwrap_or(0);
    Some((gap_start, next.timeline_start))
}

/// Click semplice, ctrl (aggiunge/toglie), shift (range col rettangolo
/// tra ancora e clip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickModifiers {
    Plain,
    Toggle,
    Range,
}

fn click_modifiers(modifiers: egui::Modifiers) -> ClickModifiers {
    if modifiers.shift {
        ClickModifiers::Range
    } else if modifiers.command {
        ClickModifiers::Toggle
    } else {
        ClickModifiers::Plain
    }
}

/// Applica un click (con eventuali modificatori) sulla clip `clicked`,
/// data la selezione e l'ancora correnti. Funzione pura, senza alcun
/// `egui::Ui`: testabile con dati semplici.
fn apply_click_selection(
    current: &BTreeSet<ClipKey>,
    anchor: Option<ClipKey>,
    clicked: ClipKey,
    modifiers: ClickModifiers,
    visuals: &[ClipVisual],
    px_per_frame: f32,
    row_y: &[f32],
) -> (BTreeSet<ClipKey>, Option<ClipKey>) {
    match modifiers {
        ClickModifiers::Plain => (BTreeSet::from([clicked]), Some(clicked)),
        ClickModifiers::Toggle => {
            let mut set = current.clone();
            if !set.remove(&clicked) {
                set.insert(clicked);
            }
            (set, Some(clicked))
        }
        ClickModifiers::Range => {
            let effective_anchor = anchor.unwrap_or(clicked);
            let anchor_rect = visuals
                .iter()
                .find(|v| (v.track_index, v.clip.id) == effective_anchor)
                .map(|v| clip_local_rect(v, px_per_frame, row_y));
            let clicked_rect = visuals
                .iter()
                .find(|v| (v.track_index, v.clip.id) == clicked)
                .map(|v| clip_local_rect(v, px_per_frame, row_y));
            let set = match (anchor_rect, clicked_rect) {
                (Some(a), Some(c)) => {
                    clips_intersecting_rect(visuals, px_per_frame, row_y, a.union(c))
                        .into_iter()
                        .collect()
                }
                _ => BTreeSet::from([clicked]),
            };
            (set, Some(effective_anchor))
        }
    }
}

/// Confini imposti dai vicini su `track_index` per una clip lunga `len`
/// posizionata (solo per decidere chi è prima/dopo) a `reference_start` —
/// non deve essere già lì: usato anche per un cambio di track a metà drag.
fn neighbor_bounds_at(
    visuals: &[ClipVisual],
    track_index: usize,
    exclude: &[ClipId],
    reference_start: FrameIdx,
    len: FrameIdx,
) -> (FrameIdx, FrameIdx) {
    let mut lower_bound: FrameIdx = 0;
    let mut upper_bound: FrameIdx = FrameIdx::MAX;
    let reference_end = reference_start + len;

    for v in visuals {
        if v.track_index != track_index || exclude.contains(&v.clip.id) {
            continue;
        }
        if v.clip.timeline_end() <= reference_start {
            lower_bound = lower_bound.max(v.clip.timeline_end());
        }
        if v.clip.timeline_start >= reference_end {
            upper_bound = upper_bound.min(v.clip.timeline_start);
        }
    }

    (lower_bound, upper_bound)
}

fn max_start_in_slot(lower: FrameIdx, upper: FrameIdx, len: FrameIdx) -> FrameIdx {
    upper.saturating_sub(len).max(lower)
}

/// Come `drag_range`, per una clip lunga `len` valutata a `reference_start`
/// su `track_index` (anche se non ci sta ancora) ignorando `exclude`.
fn drag_range_at(
    visuals: &[ClipVisual],
    track_index: usize,
    exclude: &[ClipId],
    reference_start: FrameIdx,
    len: FrameIdx,
) -> (FrameIdx, FrameIdx) {
    let (lower, upper) = neighbor_bounds_at(visuals, track_index, exclude, reference_start, len);
    (lower, max_start_in_slot(lower, upper, len))
}

/// Range valido (min/max) per il nuovo `timeline_start` di una clip da
/// sola, già risolto (non un "upper" grezzo): usato sia direttamente sia
/// come base da combinare con quello di una gemella collegata.
fn drag_range(visuals: &[ClipVisual], track_index: usize, clip_id: ClipId) -> (FrameIdx, FrameIdx) {
    let Some(v) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
    else {
        return (0, FrameIdx::MAX);
    };
    drag_range_at(
        visuals,
        track_index,
        &[clip_id],
        v.clip.timeline_start,
        v.clip.timeline_len,
    )
}

/// Estende le clip ai loro gruppi collegati. Unico punto che lo fa per le
/// selezioni fatte in timeline.
fn expand_to_linked_groups(
    visuals: &[ClipVisual],
    keys: impl IntoIterator<Item = ClipKey>,
) -> BTreeSet<ClipKey> {
    let mut result: BTreeSet<ClipKey> = BTreeSet::new();
    for (track_index, clip_id) in keys {
        let Some(visual) = visuals
            .iter()
            .find(|v| v.track_index == track_index && v.clip.id == clip_id && !v.locked)
        else {
            continue;
        };
        result.insert((track_index, clip_id));
        if let Some(group) = visual.clip.linked_group {
            for v in visuals
                .iter()
                .filter(|v| v.clip.linked_group == Some(group) && !v.locked)
            {
                result.insert((v.track_index, v.clip.id));
            }
        }
    }
    result
}

/// Clip che si muovono con `clicked`: la selezione se la contiene,
/// altrimenti lei e il suo gruppo.
fn drag_group_for(
    selected: &BTreeSet<ClipKey>,
    visuals: &[ClipVisual],
    clicked: ClipKey,
) -> BTreeSet<ClipKey> {
    if selected.contains(&clicked) {
        selected.clone()
    } else {
        expand_to_linked_groups(visuals, [clicked])
    }
}

/// Range del `timeline_start` di `clip_id` che rispetta i vincoli di tutte
/// le `others`, e i loro offset per `DragState::followers`.
fn combined_drag_range(
    visuals: &[ClipVisual],
    track_index: usize,
    clip_id: ClipId,
    others: &[ClipKey],
) -> (FrameIdx, FrameIdx, Vec<(ClipId, usize, FrameIdx)>) {
    let (mut min_start, mut max_start) = drag_range(visuals, track_index, clip_id);

    let Some(this_start) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
        .map(|v| v.clip.timeline_start)
    else {
        return (min_start, max_start, Vec::new());
    };

    let mut followers = Vec::new();
    for &(other_track, other_id) in others {
        if other_track == track_index && other_id == clip_id {
            continue;
        }
        let Some(other) = visuals
            .iter()
            .find(|v| v.track_index == other_track && v.clip.id == other_id)
        else {
            continue;
        };
        let offset = other.clip.timeline_start - this_start;
        let (o_min, o_max) = drag_range(visuals, other_track, other_id);
        // Saturante: senza vicini `o_max` è ~`FrameIdx::MAX`.
        min_start = min_start.max(o_min.saturating_sub(offset));
        max_start = max_start.min(o_max.saturating_sub(offset));
        followers.push((other_id, other_track, offset));
    }
    (min_start, max_start, followers)
}

/// Come `combined_drag_range`, con ogni clip sulla propria track target.
/// I vicini escludono tutto il gruppo, o due clip dirette sulla stessa
/// track si bloccherebbero a vicenda.
fn group_drag_bounds(
    visuals: &[ClipVisual],
    reference_start: FrameIdx,
    targets: &[(ClipId, EffectiveTrack)],
    followers: &[(ClipId, usize, FrameIdx)],
) -> (FrameIdx, FrameIdx) {
    let exclude: Vec<ClipId> = targets.iter().map(|(id, _)| *id).collect();
    let bound_for = |id: ClipId, target: EffectiveTrack, reference: FrameIdx| -> (FrameIdx, FrameIdx) {
        let len = visuals
            .iter()
            .find(|v| v.clip.id == id)
            .map(|v| v.clip.timeline_len)
            .unwrap_or(0);
        match target {
            EffectiveTrack::Existing(track) => {
                drag_range_at(visuals, track, &exclude, reference, len)
            }
            EffectiveTrack::New(_) => (0, max_start_in_slot(0, FrameIdx::MAX, len)),
        }
    };

    let (primary_id, primary_target) = targets[0];
    let (mut min_start, mut max_start) = bound_for(primary_id, primary_target, reference_start);

    for (i, &(follower_id, _, offset)) in followers.iter().enumerate() {
        let (_, follower_target) = targets[i + 1];
        let (o_min, o_max) = bound_for(follower_id, follower_target, reference_start + offset);
        // Saturante: senza vicini `o_max` è ~`FrameIdx::MAX`.
        min_start = min_start.max(o_min.saturating_sub(offset));
        max_start = max_start.min(o_max.saturating_sub(offset));
    }
    (min_start, max_start.max(min_start))
}

/// Range del bordo trimmato combinato con quello delle `others`, e i loro
/// offset per `TrimState::followers`.
fn combined_trim_range(
    visuals: &[ClipVisual],
    project: &Project,
    primary: ClipKey,
    edge: TrimEdge,
    others: &[(ClipKey, TrimEdge)],
) -> (FrameIdx, FrameIdx, Vec<(ClipId, usize, FrameIdx, TrimEdge)>) {
    let find = |(track, id): ClipKey| {
        visuals
            .iter()
            .find(|v| v.track_index == track && v.clip.id == id)
    };
    let Some(primary_visual) = find(primary) else {
        return (0, FrameIdx::MAX, Vec::new());
    };
    let edge_value = |clip: &Clip, edge: TrimEdge| match edge {
        TrimEdge::Start => clip.timeline_start,
        TrimEdge::End => clip.timeline_end(),
    };
    let primary_value = edge_value(&primary_visual.clip, edge);
    let trimmed: Vec<(&ClipVisual, TrimEdge)> = std::iter::once((primary_visual, edge))
        .chain(others.iter().filter_map(|&(k, e)| find(k).map(|v| (v, e))))
        .collect();

    let mut min_value = FrameIdx::MIN;
    let mut max_value = FrameIdx::MAX;
    let mut followers = Vec::new();
    for &(v, v_edge) in &trimmed {
        let offset = edge_value(&v.clip, v_edge) - primary_value;
        let (mut o_min, mut o_max) = single_trim_range(project, &v.clip, v_edge);
        // Due clip trimmate insieme sulla stessa track non devono
        // allungarsi l'una sopra l'altra; nel roll invece il bordo della
        // vicina si sposta con questo.
        for &(w, w_edge) in trimmed.iter().filter(|(w, w_edge)| {
            w.track_index == v.track_index && w.clip.id != v.clip.id && *w_edge == v_edge
        }) {
            match w_edge {
                TrimEdge::End if w.clip.timeline_start >= v.clip.timeline_end() => {
                    o_max = o_max.min(w.clip.timeline_start);
                }
                TrimEdge::Start if w.clip.timeline_end() <= v.clip.timeline_start => {
                    o_min = o_min.max(w.clip.timeline_end());
                }
                _ => {}
            }
        }
        min_value = min_value.max(o_min.saturating_sub(offset));
        max_value = max_value.min(o_max.saturating_sub(offset));
        if v.clip.id != primary_visual.clip.id || v.track_index != primary_visual.track_index {
            followers.push((v.clip.id, v.track_index, offset, v_edge));
        }
    }
    (min_value, max_value, followers)
}

/// Zone sensibili ai bordi di una clip larga `width` pixel, dato chi le sta
/// a contatto a sinistra (`start_neighbor`) e a destra (`end_neighbor`).
struct EdgeZones {
    width: f32,
    roll_px: f32,
    trim_px: f32,
    start_neighbor: Option<ClipKey>,
    end_neighbor: Option<ClipKey>,
}

fn edge_zones(
    width: f32,
    start_neighbor: Option<ClipKey>,
    end_neighbor: Option<ClipKey>,
) -> EdgeZones {
    EdgeZones {
        width,
        roll_px: ROLL_HANDLE_PX.min(width / 6.0),
        trim_px: TRIM_HANDLE_PX.min(width / 3.0),
        start_neighbor,
        end_neighbor,
    }
}

impl EdgeZones {
    /// `local_x`: distanza dal bordo sinistro della clip.
    fn at(&self, local_x: f32) -> Option<EdgeZone> {
        let sides = [
            (local_x, TrimEdge::Start, self.start_neighbor),
            (self.width - local_x, TrimEdge::End, self.end_neighbor),
        ];
        for (distance, edge, neighbor) in sides {
            match neighbor {
                Some(neighbor) if distance < self.roll_px => {
                    return Some(EdgeZone::Roll { edge, neighbor });
                }
                Some(_) if distance < self.roll_px + self.trim_px => {
                    return Some(EdgeZone::Trim(edge));
                }
                None if distance < self.trim_px => return Some(EdgeZone::Trim(edge)),
                _ => {}
            }
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EdgeCursor {
    TrimStart,
    TrimEnd,
    Roll,
}

impl EdgeCursor {
    fn from_zone(zone: EdgeZone) -> Self {
        match zone {
            EdgeZone::Roll { .. } => EdgeCursor::Roll,
            EdgeZone::Trim(TrimEdge::Start) => EdgeCursor::TrimStart,
            EdgeZone::Trim(TrimEdge::End) => EdgeCursor::TrimEnd,
        }
    }
}

/// egui non ha cursori personalizzati: si nasconde quello di sistema e si
/// disegna questo al suo posto. Parentesi "[" / "]" come i bordi di una
/// clip, con le frecce del trascinamento.
fn paint_edge_cursor(ctx: &egui::Context, pos: egui::Pos2, cursor: EdgeCursor) {
    ctx.set_cursor_icon(egui::CursorIcon::None);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("timeline_edge_cursor"),
    ));
    const HALF_H: f32 = 8.0;
    const TICK: f32 = 4.0;
    let bracket = |x: f32, towards: f32| {
        vec![
            egui::pos2(x + towards * TICK, pos.y - HALF_H),
            egui::pos2(x, pos.y - HALF_H),
            egui::pos2(x, pos.y + HALF_H),
            egui::pos2(x + towards * TICK, pos.y + HALF_H),
        ]
    };
    // (tip x, direzione)
    let arrow = |tip: f32, dir: f32| {
        vec![
            egui::pos2(tip, pos.y),
            egui::pos2(tip - dir * 5.0, pos.y - 4.5),
            egui::pos2(tip - dir * 5.0, pos.y + 4.5),
        ]
    };
    let (brackets, arrows) = match cursor {
        EdgeCursor::TrimEnd => (vec![bracket(pos.x, -1.0)], vec![arrow(pos.x - 11.0, -1.0), arrow(pos.x + 8.0, 1.0)]),
        EdgeCursor::TrimStart => (vec![bracket(pos.x, 1.0)], vec![arrow(pos.x - 8.0, -1.0), arrow(pos.x + 11.0, 1.0)]),
        EdgeCursor::Roll => (
            vec![bracket(pos.x - 2.0, -1.0), bracket(pos.x + 2.0, 1.0)],
            vec![arrow(pos.x - 10.0, -1.0), arrow(pos.x + 10.0, 1.0)],
        ),
    };
    for points in &brackets {
        painter.line(points.clone(), egui::Stroke::new(4.0, egui::Color32::BLACK));
    }
    for points in brackets {
        painter.line(points, egui::Stroke::new(2.0, egui::Color32::WHITE));
    }
    for points in arrows {
        painter.add(egui::Shape::convex_polygon(
            points,
            egui::Color32::WHITE,
            egui::Stroke::new(1.0, egui::Color32::BLACK),
        ));
    }
}

/// I vicini non limitano il trim (li si sovrascrive al rilascio): solo il
/// sorgente e il bordo opposto.
fn single_trim_range(project: &Project, clip: &Clip, edge: TrimEdge) -> (FrameIdx, FrameIdx) {
    match edge {
        TrimEdge::Start => {
            // Non oltre la fine meno 1 frame (deve restare almeno un frame
            // di contenuto) e non prima dell'inizio del sorgente
            // (source_in non può scendere sotto 0).
            let min_value = clip.timeline_frame_at(0).max(0);
            let max_value = clip.timeline_end() - 1;
            (min_value, max_value.max(min_value))
        }
        TrimEdge::End => {
            // Non oltre l'inizio più 1 frame e non oltre la durata reale
            // del sorgente (illimitata per un generatore SolidColor, che
            // non ne ha una).
            let max_value = media_duration_frames(project, clip)
                .map(|max_source_out| clip.timeline_frame_at(max_source_out))
                .unwrap_or(FrameIdx::MAX);
            let min_value = clip.timeline_start + 1;
            (min_value, max_value.max(min_value))
        }
    }
}

fn media_duration_frames(project: &Project, clip: &Clip) -> Option<FrameIdx> {
    match &clip.source {
        ClipSource::Media(media_id) => project
            .media_pool
            .get(*media_id)
            .map(|item| item.meta.duration_frames),
        ClipSource::SolidColor | ClipSource::Text => None,
    }
}

/// Il tratto di timeline che una clip si prende allungando `edge` fino a
/// `new_value`, se si è allungata: `None` se l'ha invece accorciata.
fn grown_range(
    clip: &Clip,
    track_index: usize,
    edge: TrimEdge,
    new_value: FrameIdx,
) -> Option<(usize, FrameIdx, FrameIdx)> {
    match edge {
        TrimEdge::Start if new_value < clip.timeline_start => {
            Some((track_index, new_value, clip.timeline_start))
        }
        TrimEdge::End if new_value > clip.timeline_end() => {
            Some((track_index, clip.timeline_end(), new_value))
        }
        _ => None,
    }
}

/// Soglia di aggancio della calamita, in pixel schermo (non in frame:
/// resta la stessa distanza visiva a qualunque livello di zoom, convertita
/// in frame da `snap_frame` in base a `px_per_frame`).
const SNAP_THRESHOLD_PX: f32 = 10.0;

/// Bordi delle clip non escluse più `extra_targets` (la testina).
fn snap_targets<'a>(
    visuals: &'a [ClipVisual],
    exclude: &'a [ClipId],
    extra_targets: &'a [FrameIdx],
) -> impl Iterator<Item = FrameIdx> + 'a {
    visuals
        .iter()
        .filter(|v| !exclude.contains(&v.clip.id))
        .flat_map(|v| [v.clip.timeline_start, v.clip.timeline_end()])
        .chain(extra_targets.iter().copied())
}

/// Con la calamita attiva aggancia l'inizio o la fine della clip lunga
/// `len` al bordo più vicino entro `SNAP_THRESHOLD_PX`. `exclude` non conta.
fn snap_frame(
    candidate_start: FrameIdx,
    len: FrameIdx,
    visuals: &[ClipVisual],
    exclude: &[ClipId],
    extra_targets: &[FrameIdx],
    px_per_frame: f32,
    enabled: bool,
) -> FrameIdx {
    if !enabled {
        return candidate_start;
    }
    let threshold = (SNAP_THRESHOLD_PX / px_per_frame).round() as FrameIdx;
    if threshold <= 0 {
        return candidate_start;
    }
    let candidate_end = candidate_start + len;

    let mut best: Option<(FrameIdx, FrameIdx)> = None; // (|scarto|, nuovo candidate_start)
    for edge in snap_targets(visuals, exclude, extra_targets) {
        // (punto della clip trascinata da confrontare col bordo, nuovo
        // candidate_start se questo è l'aggancio scelto)
        for (point, new_start) in [(candidate_start, edge), (candidate_end, edge - len)] {
            let delta = (point - edge).abs();
            if delta > threshold {
                continue;
            }
            if best.is_none_or(|(best_delta, _)| delta < best_delta) {
                best = Some((delta, new_start));
            }
        }
    }
    best.map_or(candidate_start, |(_, new_start)| new_start)
}

/// Evidenzia una zona "nuova track" sotto un drag in corso.
fn paint_drop_zone(painter: &egui::Painter, rect: egui::Rect, label: Option<&str>) {
    let green = egui::Color32::from_rgb(120, 220, 120);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgba_unmultiplied(120, 220, 120, 60));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(2.0, green), egui::StrokeKind::Inside);
    if let Some(label) = label {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(200, 255, 200),
        );
    }
}

const PROXY_STRIP_HEIGHT: f32 = 4.0;
/// Clip il cui media non è più nel media pool.
pub const OFFLINE_COLOR: egui::Color32 = egui::Color32::from_rgb(170, 50, 50);

/// Indicatore "proxy disponibile", condiviso col media pool.
pub const PROXY_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(220, 151, 52, 220);

/// Bordo della clip mentre ci si trascina sopra un filtro del pannello Effects.
const FILTER_HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);

fn paint_proxy_strip(painter: &egui::Painter, rect: egui::Rect) {
    let strip_rect = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(1.0, 1.0),
        egui::vec2(rect.width() - 2.0, PROXY_STRIP_HEIGHT),
    );
    painter.rect_filled(strip_rect, 2.0, PROXY_COLOR);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug segnalato: dividere una clip audio (`SplitClip`) spostava
    /// visibilmente la forma d'onda disegnata esattamente nel punto di
    /// taglio, perché ogni metà arrotondava i propri bin ai suoi bordi.
    /// Con `waveform_bin_for_column` calcolato dalla posizione *assoluta*
    /// nel tempo audio, lo stesso istante deve mappare sempre allo stesso
    /// bin sia prima sia dopo la divisione.
    #[test]
    fn waveform_bin_for_column_is_continuous_across_a_clip_split() {
        // Valori realistici da un caso reale (bbb_sunflower): fps video e
        // durata audio del *media* leggermente diversi dalla durata del
        // video, la causa dell'arrotondamento che il bug esponeva.
        let media_fps = 60.0_f64;
        let full_source_out: FrameIdx = 38074;
        let audio_duration_secs = 634.144; // leggermente < 38074/60.0
        let num_peaks = 63457;

        let split_at: FrameIdx = 3120; // 52s a 60fps

        for probe_secs in [51.5, 51.9, 52.0, 52.1, 52.5, 53.0] {
            // Bin secondo la clip intera (non divisa): source_in=0,
            // source_out=full_source_out.
            let whole_clip_start = 0.0_f64;
            let whole_clip_end = full_source_out as f64 / media_fps;
            let frac_whole = probe_secs / whole_clip_end;
            let bin_whole = waveform_bin_for_column(
                frac_whole,
                whole_clip_start,
                whole_clip_end,
                audio_duration_secs,
                num_peaks,
            );

            // Stesso istante, ma dalla metà (sinistra o destra) prodotta da
            // uno split a `split_at`.
            let (clip_start_frame, clip_end_frame) = if probe_secs * media_fps < split_at as f64 {
                (0, split_at)
            } else {
                (split_at, full_source_out)
            };
            let clip_start_secs = clip_start_frame as f64 / media_fps;
            let clip_end_secs = clip_end_frame as f64 / media_fps;
            let frac_half = (probe_secs - clip_start_secs) / (clip_end_secs - clip_start_secs);
            let bin_half = waveform_bin_for_column(
                frac_half,
                clip_start_secs,
                clip_end_secs,
                audio_duration_secs,
                num_peaks,
            );

            assert_eq!(
                bin_whole, bin_half,
                "a t={probe_secs}s la clip intera sceglie il bin {bin_whole} ma la metà dopo lo split sceglie {bin_half}: la forma d'onda si sposterebbe al taglio"
            );
        }
    }

    /// Deve restare tra le prime candidate (1-2-5) a bassissimo zoom, e
    /// salire abbastanza da tenere le tacche leggibili anche a zoom
    /// molto alto — non un valore fisso qualunque sia `pixels_per_sec`.
    #[test]
    fn nice_tick_interval_secs_grows_with_zoom_to_keep_ticks_readable() {
        assert_eq!(nice_tick_interval_secs(800.0), 1.0);
        assert_eq!(nice_tick_interval_secs(60.0), 2.0);
        assert_eq!(nice_tick_interval_secs(10.0), 10.0);
        assert_eq!(nice_tick_interval_secs(0.1), 900.0);
    }

    #[test]
    fn format_timecode_includes_frames_and_hours() {
        // 25 fps: 5 secondi = frame 125, mostra HH:MM:SS:FF
        assert_eq!(format_timecode(5.0, 25.0), "00:00:05:00");
        // 65 secondi = 1 min 5 sec
        assert_eq!(format_timecode(65.0, 25.0), "00:01:05:00");
        // 3665 secondi = 1h 1m 5s
        assert_eq!(format_timecode(3665.0, 25.0), "01:01:05:00");
        // Con frazione di secondo: 0.4s a 25fps = frame 10
        assert_eq!(format_timecode(5.4, 25.0), "00:00:05:10");
        // 29,97: 30 frame per secondo nominale, non 29.
        assert_eq!(format_timecode(1799.0 / (30_000.0 / 1001.0), 30_000.0 / 1001.0), "00:00:59:29");
    }

    #[test]
    fn format_duration_is_seconds_and_leftover_frames() {
        assert_eq!(format_duration(0, 10.0), "0:00");
        // 10 frame a 10fps = 1s esatto, niente frame residui.
        assert_eq!(format_duration(10, 10.0), "1:00");
        // 14 frame a 10fps = 1s + 4 frame.
        assert_eq!(format_duration(14, 10.0), "1:04");
        assert_eq!(format_duration(93, 30.0), "3:03");
    }

    #[test]
    fn fade_zone_at_only_matches_the_top_band_near_the_handle() {
        let clip_rect = egui::Rect::from_min_size(egui::pos2(100.0, 20.0), egui::vec2(200.0, 40.0));
        let fade_in_x = 120.0; // handle di fade-in a 20px dal bordo sx
        let fade_out_x = 280.0; // handle di fade-out a 20px dal bordo dx
        assert_eq!(
            fade_zone_at(egui::pos2(120.0, 22.0), clip_rect, fade_in_x, fade_out_x),
            Some(FadeEdge::In)
        );
        assert_eq!(
            fade_zone_at(egui::pos2(280.0, 22.0), clip_rect, fade_in_x, fade_out_x),
            Some(FadeEdge::Out)
        );
        // Stessa X dell'handle di fade-in, ma sotto la banda in alto: è trim/roll, non fade.
        assert_eq!(fade_zone_at(egui::pos2(120.0, 50.0), clip_rect, fade_in_x, fade_out_x), None);
        // Lontano da entrambi gli handle.
        assert_eq!(fade_zone_at(egui::pos2(200.0, 22.0), clip_rect, fade_in_x, fade_out_x), None);
    }

    #[test]
    fn fade_drag_value_moves_in_opposite_screen_directions_for_in_and_out() {
        let px_per_frame = 2.0;
        let drag_right = |edge| FadeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            edge,
            original_value: 10,
            accum_px: 20.0, // 20px a destra = 10 frame
        };
        // Fade-in: trascinare a destra allunga la dissolvenza.
        assert_eq!(fade_drag_value(&drag_right(FadeEdge::In), 100, px_per_frame), 20);
        // Fade-out: lo stesso movimento a destra la accorcia (l'handle si
        // avvicina all'angolo).
        assert_eq!(fade_drag_value(&drag_right(FadeEdge::Out), 100, px_per_frame), 0);
        // Clampata alla durata della clip.
        let far = FadeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            edge: FadeEdge::In,
            original_value: 10,
            accum_px: 1000.0,
        };
        assert_eq!(fade_drag_value(&far, 30, px_per_frame), 30);
    }

    #[test]
    fn crossing_drag_value_grows_when_the_grabbed_extremity_moves_away_from_the_cut() {
        let px_per_frame = 2.0;
        // Estremità sinistra trascinata più a sinistra (lontano dal
        // taglio, verso l'interno della clip di sinistra): la crossing si
        // allunga, il doppio dei frame spostati (cresce su entrambi i
        // lati insieme).
        let left_extends = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 100,
            accum_px: -20.0, // 20px a sinistra = 10 frame
        };
        assert_eq!(crossing_drag_value(&left_extends, px_per_frame), 30);
        // Stesso spostamento in pixel ma sul lato destro, verso destra:
        // stesso effetto, si allontana dal taglio nella direzione opposta.
        let right_extends = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: false,
            original_duration: 10,
            max_duration: 100,
            accum_px: 20.0,
        };
        assert_eq!(crossing_drag_value(&right_extends, px_per_frame), 30);
        // Trascinare l'estremità sinistra verso destra (verso il taglio)
        // la accorcia, clampata a un minimo di 1 frame.
        let shrinking = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 100,
            accum_px: 20.0,
        };
        assert_eq!(crossing_drag_value(&shrinking, px_per_frame), 1);
        // Clampata al massimo consentito dalle due clip coinvolte.
        let far = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 40,
            accum_px: -1000.0,
        };
        assert_eq!(crossing_drag_value(&far, px_per_frame), 40);
    }

    #[test]
    fn gain_offset_is_zero_at_zero_db_and_reaches_the_edges_at_the_range_extremes() {
        assert_eq!(gain_offset(0.0), 0.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MAX), 1.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MIN), -1.0);
        // Oltre gli estremi resta clampato, non sfora [-1, 1].
        assert_eq!(gain_offset(vv_core::GAIN_DB_MAX + 10.0), 1.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MIN - 10.0), -1.0);
    }

    #[test]
    fn gain_from_offset_is_the_inverse_of_gain_offset() {
        for db in [vv_core::GAIN_DB_MIN, -50.0, -6.0, 0.0, 6.0, vv_core::GAIN_DB_MAX] {
            assert!((gain_from_offset(gain_offset(db)) - db).abs() < 1e-4, "db={db}");
        }
    }

    #[test]
    fn volume_drag_value_follows_the_pointer_and_clamps_at_the_range_extremes() {
        let half_height = 18.0; // (ROW_HEIGHT - 4.0) / 2.0
        let group = History::default().begin_group();
        let drag = |accum_px| VolumeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            original_db: 0.0,
            accum_px,
            group,
        };
        // A 0 dB la riga è al centro: trascinare verso l'alto (accum_px
        // negativo) alza il gain, verso il basso lo abbassa.
        assert!(volume_drag_value(&drag(-half_height), half_height) > 0.0);
        assert!(volume_drag_value(&drag(half_height), half_height) < 0.0);
        // Oltre la corsa disponibile si clampa agli estremi del range.
        assert_eq!(volume_drag_value(&drag(-half_height * 10.0), half_height), vv_core::GAIN_DB_MAX);
        assert_eq!(volume_drag_value(&drag(half_height * 10.0), half_height), vv_core::GAIN_DB_MIN);
    }

    /// `row_y` "identità" (nessun raggruppamento/margine) per i test.
    fn test_row_y(n: usize) -> Vec<f32> {
        (0..n).map(|i| RULER_HEIGHT + i as f32 * ROW_HEIGHT).collect()
    }

    fn visual(track_index: usize, id: u64, start: FrameIdx, len: FrameIdx) -> ClipVisual<'static> {
        ClipVisual {
            track_index,
            clip: std::borrow::Cow::Owned(Clip::from_source_range(
                ClipId(id),
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            )),
            label: String::new(),
            color: egui::Color32::WHITE,
            locked: false,
            muted: false,
        }
    }

    fn visual_linked(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        len: FrameIdx,
        group: u64,
    ) -> ClipVisual<'static> {
        let mut v = visual(track_index, id, start, len);
        v.clip.to_mut().linked_group = Some(vv_core::LinkGroupId(group));
        v
    }

    #[test]
    fn expand_to_linked_groups_includes_the_whole_group() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 100), visual_linked(1, 2, 0, 10, 100)];
        assert_eq!(
            expand_to_linked_groups(&visuals, [(0, ClipId(1))]),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]),
            "selezionando il video deve espandere anche all'audio collegato"
        );
        assert_eq!(
            expand_to_linked_groups(&visuals, [(1, ClipId(2))]),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]),
            "e viceversa, partendo dall'audio"
        );
    }

    #[test]
    fn expand_to_linked_groups_is_a_noop_for_unlinked_clips() {
        let visuals = vec![visual(0, 1, 0, 10)];
        assert_eq!(
            expand_to_linked_groups(&visuals, [(0, ClipId(1))]),
            BTreeSet::from([(0, ClipId(1))])
        );
        assert_eq!(expand_to_linked_groups(&visuals, []), BTreeSet::new());
    }

    #[test]
    fn expand_to_linked_groups_handles_independent_groups_and_groups_larger_than_two() {
        // Un gruppo da 2 e uno da 3, indipendenti, entrambi punto di
        // partenza: l'intero gruppo di ciascuno deve comparire nel
        // risultato, non solo un partner.
        let visuals = vec![
            visual_linked(0, 1, 0, 10, 100),
            visual_linked(1, 2, 0, 10, 100),
            visual_linked(0, 3, 20, 10, 200),
            visual_linked(1, 4, 20, 10, 200),
            visual_linked(2, 5, 20, 10, 200),
        ];
        let result = expand_to_linked_groups(&visuals, [(0, ClipId(1)), (0, ClipId(3))]);
        assert_eq!(
            result,
            BTreeSet::from([
                (0, ClipId(1)),
                (1, ClipId(2)),
                (0, ClipId(3)),
                (1, ClipId(4)),
                (2, ClipId(5)),
            ])
        );
    }

    /// Bug segnalato: con CTRL+click/rettangolo selezionavo 2+ clip *non*
    /// collegate tra loro, poi trascinandone una le altre non seguivano —
    /// il drag guardava solo il gruppo collegato della clip cliccata,
    /// ignorando il resto della selezione.
    #[test]
    fn drag_group_for_follows_the_whole_multi_selection_even_without_a_link() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(1, 2, 30, 10),
            visual(2, 3, 60, 10),
        ];
        // 3 clip non collegate tra loro, tutte selezionate a mano (CTRL+click).
        let selected = BTreeSet::from([(0, ClipId(1)), (1, ClipId(2)), (2, ClipId(3))]);

        // Trascinandone una qualunque, il drag deve seguire l'intera
        // selezione — non solo lei.
        assert_eq!(
            drag_group_for(&selected, &visuals, (1, ClipId(2))),
            selected
        );
    }

    #[test]
    fn drag_group_for_replaces_the_selection_when_dragging_an_unselected_clip() {
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 30, 10)];
        // Selezione precedente e scorrelata: trascinare una clip fuori da
        // essa non deve trascinarsela dietro.
        let selected = BTreeSet::from([(0, ClipId(1))]);
        assert_eq!(
            drag_group_for(&selected, &visuals, (1, ClipId(2))),
            BTreeSet::from([(1, ClipId(2))])
        );
    }

    #[test]
    fn drag_group_for_expands_to_the_link_group_when_dragging_an_unselected_linked_clip() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 100), visual_linked(1, 2, 0, 10, 100)];
        let selected = BTreeSet::new();
        assert_eq!(
            drag_group_for(&selected, &visuals, (0, ClipId(1))),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))])
        );
    }

    #[test]
    fn apply_click_selection_plain_replaces_selection() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, anchor) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(2)),
            ClickModifiers::Plain,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(2))]));
        assert_eq!(anchor, Some((0, ClipId(2))));
    }

    #[test]
    fn apply_click_selection_toggle_adds_and_removes() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, _) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(2)),
            ClickModifiers::Toggle,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1)), (0, ClipId(2))]));

        // Ctrl+click su una clip già selezionata la rimuove.
        let (selected2, _) = apply_click_selection(
            &selected,
            Some((0, ClipId(2))),
            (0, ClipId(1)),
            ClickModifiers::Toggle,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected2, BTreeSet::from([(0, ClipId(2))]));
    }

    #[test]
    fn apply_click_selection_range_selects_bounding_box_from_anchor() {
        // Tre clip sulla stessa track: [0,10) [20,30) [40,50). Ancora=1,
        // shift+click su 3 deve selezionare anche la 2 in mezzo.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 10),
            visual(0, 3, 40, 10),
        ];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, anchor) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(3)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(1),
        );
        assert_eq!(
            selected,
            BTreeSet::from([(0, ClipId(1)), (0, ClipId(2)), (0, ClipId(3))])
        );
        // L'ancora non cambia con shift+click.
        assert_eq!(anchor, Some((0, ClipId(1))));
    }

    #[test]
    fn apply_click_selection_range_spans_multiple_tracks() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // video, ancora
            visual(1, 2, 0, 10),  // audio, dentro al range (stessa colonna)
            visual(0, 3, 20, 10), // fuori dal range orizzontale
        ];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, _) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (1, ClipId(2)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]));
    }

    #[test]
    fn apply_click_selection_range_without_prior_anchor_uses_clicked_as_anchor() {
        let visuals = vec![visual(0, 1, 0, 10)];
        let current = BTreeSet::new();
        let (selected, anchor) = apply_click_selection(
            &current,
            None,
            (0, ClipId(1)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(1),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1))]));
        assert_eq!(anchor, Some((0, ClipId(1))));
    }

    #[test]
    fn clips_intersecting_rect_finds_overlapping_clips_only() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 10),
            visual(1, 3, 0, 10),
        ];
        // Rettangolo che copre solo l'area della clip 1 e 3 (colonna
        // iniziale, entrambe le track), non la 2.
        let row_y = test_row_y(2);
        let rect =
            clip_local_rect(&visuals[0], 1.0, &row_y).union(clip_local_rect(&visuals[2], 1.0, &row_y));
        let hits: BTreeSet<_> = clips_intersecting_rect(&visuals, 1.0, &row_y, rect)
            .into_iter()
            .collect();
        assert_eq!(hits, BTreeSet::from([(0, ClipId(1)), (1, ClipId(3))]));
    }

    #[test]
    fn neighbor_bounds_no_neighbors_is_unbounded() {
        let visuals = vec![visual(0, 1, 10, 5)];
        assert_eq!(
            neighbor_bounds_at(&visuals, 0, &[ClipId(1)], 10, 5),
            (0, FrameIdx::MAX)
        );
    }

    #[test]
    fn neighbor_bounds_clamped_by_prev_and_next_on_same_track() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // finisce a 10
            visual(0, 2, 20, 30), // il moving
            visual(0, 3, 50, 5),  // inizia a 50
            visual(1, 4, 15, 3),  // altra track: ignorata
        ];
        assert_eq!(neighbor_bounds_at(&visuals, 0, &[ClipId(2)], 20, 30), (10, 50));
    }

    #[test]
    fn max_start_in_slot_keeps_clip_inside_slot() {
        // slot [10, 50), clip lunga 30: può stare solo tra 10 e 20.
        assert_eq!(0.clamp(10, max_start_in_slot(10, 50, 30)), 10);
        assert_eq!(15.clamp(10, max_start_in_slot(10, 50, 30)), 15);
        assert_eq!(100.clamp(10, max_start_in_slot(10, 50, 30)), 20);
    }

    #[test]
    fn max_start_in_slot_degenerate_slot_does_not_invert_range() {
        // slot più piccolo della clip: non deve produrre un range invertito.
        assert_eq!(max_start_in_slot(10, 15, 30), 10);
    }

    #[test]
    fn drag_range_matches_neighbor_bounds_minus_own_length() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // finisce a 10
            visual(0, 2, 20, 30), // lunga 30: può stare tra 10 e 50-30=20
            visual(0, 3, 50, 5),
        ];
        assert_eq!(drag_range(&visuals, 0, ClipId(2)), (10, 20));
    }

    #[test]
    fn combined_drag_range_with_no_others_matches_plain_drag_range() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 30)];
        let (min, max, followers) = combined_drag_range(&visuals, 0, ClipId(2), &[]);
        assert_eq!((min, max), drag_range(&visuals, 0, ClipId(2)));
        assert!(followers.is_empty());
    }

    #[test]
    fn combined_drag_range_intersects_both_clips_constraints() {
        // Track 0: [0,10) poi la clip 2 (video, [20,50)).
        // Track 1: la sua gemella (audio, stesso [20,50)) ma con un
        // vicino successivo più stretto: finisce a 55 invece che libero.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 30), // video, collegata a 3
            visual(1, 3, 20, 30), // audio, collegata a 2
            visual(1, 4, 55, 5),  // vincola la gemella audio a stare <= 55-30=25
        ];
        // Da sola track 0 permetterebbe [10, MAX-30]; la gemella sulla
        // track 1 la restringe a max_start <= 25 (stesso offset, 0).
        let (min, max, followers) = combined_drag_range(&visuals, 0, ClipId(2), &[(1, ClipId(3))]);
        assert_eq!(min, 10);
        assert_eq!(max, 25);
        assert_eq!(followers, vec![(ClipId(3), 1, 0)]);
    }

    #[test]
    fn combined_drag_range_respects_nonzero_offset_between_linked_clips() {
        // La gemella non è allineata: parte 5 frame dopo la primaria.
        let visuals = vec![
            visual(0, 1, 10, 20), // primaria, track 0, start=10
            visual(1, 2, 15, 20), // gemella, track 1, start=15 (offset=5)
            visual(1, 3, 60, 5),  // vincola la gemella: max_start <= 60-20=40
        ];
        let (_, max, followers) = combined_drag_range(&visuals, 0, ClipId(1), &[(1, ClipId(2))]);
        // vincolo gemella tradotto: primaria.max_start <= 40 - offset(5) = 35
        assert_eq!(max, 35);
        assert_eq!(followers, vec![(ClipId(2), 1, 5)]);
    }

    #[test]
    fn combined_drag_range_intersects_a_group_of_three() {
        // Gruppo da 3 su 3 track diverse, ognuna con un vincolo diverso.
        let visuals = vec![
            visual(0, 1, 10, 10), // primaria, track 0, start=10, nessun vicino
            visual(1, 2, 10, 10), // stesso start, vincolata da un vicino a 25
            visual(1, 5, 35, 5),
            visual(2, 3, 10, 10), // stesso start, vincolata da un vicino a 22
            visual(2, 6, 32, 5),
        ];
        let (min, max, followers) =
            combined_drag_range(&visuals, 0, ClipId(1), &[(1, ClipId(2)), (2, ClipId(3))]);
        assert_eq!(min, 0);
        // track1: max_start <= 35-10=25; track2: max_start <= 32-10=22 (più stretto)
        assert_eq!(max, 22);
        assert_eq!(
            followers,
            vec![(ClipId(2), 1, 0), (ClipId(3), 2, 0)],
            "entrambe le altre clip del gruppo, con offset 0 (stesso start)"
        );
    }

    #[test]
    fn drag_range_at_checks_neighbors_on_a_track_the_clip_isnt_on() {
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 20, 10)];
        // Clip 1 valutata come se stesse per atterrare sulla track 1: deve
        // rispettare il vicino lì (clip 2), non quelli della sua track reale.
        assert_eq!(drag_range_at(&visuals, 1, &[ClipId(1)], 5, 10), (0, 10));
    }

    #[test]
    fn group_drag_bounds_intersects_primary_and_followers_on_their_own_targets() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(1, 2, 0, 10),
            visual(1, 3, 30, 10),
        ];
        let targets = vec![
            (ClipId(1), EffectiveTrack::Existing(0)),
            (ClipId(2), EffectiveTrack::Existing(1)),
        ];
        let followers = vec![(ClipId(2), 1, -5)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 0, &targets, &followers);
        assert_eq!((min_start, max_start), (5, 25));
    }

    #[test]
    fn group_drag_bounds_lets_group_members_land_on_the_same_track_without_blocking_each_other() {
        // Due clip del gruppo (1 e 2) atterrano entrambe sulla track 1: non
        // devono bloccarsi a vicenda, solo la clip estranea (9) conta come
        // vicino.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(2, 2, 5, 10),
            visual(1, 9, 50, 5),
        ];
        let targets = vec![
            (ClipId(1), EffectiveTrack::Existing(1)),
            (ClipId(2), EffectiveTrack::Existing(1)),
        ];
        let followers = vec![(ClipId(2), 2, 5)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 0, &targets, &followers);
        assert_eq!((min_start, max_start), (0, 35));
    }

    #[test]
    fn dragging_from_the_middle_of_a_group_onto_an_empty_track_does_not_overflow() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10), visual(0, 3, 40, 10)];
        let targets = vec![
            (ClipId(2), EffectiveTrack::Existing(1)),
            (ClipId(1), EffectiveTrack::Existing(1)),
            (ClipId(3), EffectiveTrack::New(1)),
        ];
        let followers = vec![(ClipId(1), 0, -20), (ClipId(3), 0, 20)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 20, &targets, &followers);
        assert_eq!(min_start, 20);
        assert!(max_start > 1_000_000);

        let (min_start, _, _) = combined_drag_range(&visuals, 0, ClipId(2), &[(0, ClipId(1)), (0, ClipId(3))]);
        assert_eq!(min_start, 20);
    }

    /// 2 track video e 1 audio; 228px sotto al righello = 50px di zona
    /// vuota sopra e sotto ai gruppi, 0 = nessuna zona vuota.
    fn test_layout(slack: f32) -> PaneLayout {
        let avail = 2.0 * slack + 3.0 * ROW_HEIGHT + GROUP_DIVIDER_HEIGHT;
        PaneLayout::new(avail, 2, 1, &mut TimelineState::default())
    }

    #[test]
    fn track_drag_target_above_video_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2]; // 2 track video (decrescente), 1 audio
        let target = track_drag_target(40.0, TrackKind::Video, &row_order, &test_layout(50.0));
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_above_video_group_without_margin_is_the_top_track() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(15.0, TrackKind::Video, &row_order, &test_layout(0.0));
        assert!(matches!(target, Some(TrackDragTarget::Track(1))));
    }

    #[test]
    fn track_drag_target_lands_on_the_right_video_row() {
        let row_order = [1, 0, 2];
        let layout = test_layout(50.0);
        let first_row = track_drag_target(80.0, TrackKind::Video, &row_order, &layout);
        assert!(matches!(first_row, Some(TrackDragTarget::Track(1))));
        let second_row = track_drag_target(120.0, TrackKind::Video, &row_order, &layout);
        assert!(matches!(second_row, Some(TrackDragTarget::Track(0))));
    }

    #[test]
    fn track_drag_target_video_over_audio_group_is_none() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(170.0, TrackKind::Video, &row_order, &test_layout(50.0));
        assert!(target.is_none());
    }

    /// Un effetto (sempre video) o un media con video sopra una track
    /// video libera atterrano esattamente lì, non sulla prima libera —
    /// lo stesso identico criterio, indipendentemente da quale dei due
    /// si stia trascinando.
    #[test]
    fn media_pool_drop_target_lands_on_the_hovered_video_track_for_a_generator_or_a_video_media() {
        for has_video in [true, false] {
            let target = media_pool_drop_target(2, has_video, TrackKind::Video, false);
            if has_video {
                assert_eq!(target, Some(MediaDropTarget::Track(2)));
            } else {
                // Un media senza video (audio-only) su una track video
                // non forza quella track: ricade sulla risoluzione di
                // sempre, come già succedeva.
                assert_eq!(target, Some(MediaDropTarget::Default));
            }
        }
    }

    #[test]
    fn media_pool_drop_target_over_an_audio_track_falls_back_to_default() {
        assert_eq!(
            media_pool_drop_target(0, true, TrackKind::Audio, false),
            Some(MediaDropTarget::Default)
        );
    }

    #[test]
    fn media_pool_drop_target_over_a_locked_track_refuses_the_drop() {
        assert_eq!(media_pool_drop_target(2, true, TrackKind::Video, true), None);
    }

    #[test]
    fn track_drag_target_below_audio_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, &test_layout(50.0));
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_below_audio_group_without_margin_is_the_bottom_track() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, &test_layout(0.0));
        assert!(matches!(target, Some(TrackDragTarget::Track(2))));
    }

    /// Bug segnalato: con molte track video il separatore non saliva oltre
    /// la track più in alto, quindi non si poteva fare spazio all'audio.
    #[test]
    fn divider_can_shrink_an_overflowing_video_pane_which_then_scrolls() {
        let mut state = TimelineState::default();
        let avail = 200.0;
        let unconstrained = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(unconstrained.video_height_range.min, MIN_PANE_HEIGHT);

        state.video_pane_height = Some(60.0);
        let layout = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(layout.video_height(), 60.0);
        // Appoggiate al separatore finché non si scorre.
        assert_eq!(layout.video_rows_bottom(), layout.video_pane.max);
        assert_eq!(layout.video_max_scroll, 6.0 * ROW_HEIGHT + NEW_TRACK_ZONE_HEIGHT - 60.0);

        state.video_scroll = 10_000.0;
        let scrolled = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(state.video_scroll, layout.video_max_scroll);
        assert_eq!(scrolled.video_rows_top, scrolled.video_pane.min + NEW_TRACK_ZONE_HEIGHT);
    }

    #[test]
    fn audio_pane_scrolls_when_its_tracks_overflow() {
        let mut state = TimelineState::default();
        state.audio_scroll = 10_000.0;
        let layout = PaneLayout::new(200.0, 1, 8, &mut state);
        assert!(layout.audio_max_scroll > 0.0);
        assert_eq!(
            layout.audio_rows_bottom() + NEW_TRACK_ZONE_HEIGHT,
            layout.audio_pane.max
        );
        let last_row = layout.row_at_y(layout.audio_pane.max - NEW_TRACK_ZONE_HEIGHT - 1.0);
        assert_eq!(last_row, 8);
    }

    #[test]
    fn drag_group_row_targets_shifts_a_same_kind_follower_by_the_same_amount() {
        // track_kinds: [Video, Audio, Video, Video] -> row_order [3,2,0,1]
        // (video decrescente, audio crescente), row_of_track [2,3,1,0].
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 3, 1, 0];
        let row_order = [3, 2, 0, 1];
        // Primaria (track 2, riga 1) sale di una riga -> track 3 (riga 0).
        // Follower video (track 0, riga 2, "sotto" la primaria) deve
        // scattare nella riga appena lasciata libera dalla primaria (riga
        // 1 -> track 2), esattamente come C1 segue C2 nell'esempio
        // dell'utente.
        let followers = vec![(ClipId(9), 0, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            2,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            4,
        );
        assert_eq!(
            targets,
            vec![
                (ClipId(1), EffectiveTrack::Existing(3)),
                (ClipId(9), EffectiveTrack::Existing(2)),
            ]
        );
    }

    #[test]
    fn drag_group_row_targets_moves_an_audio_follower_in_the_opposite_row_direction() {
        // track_kinds: [Video, Audio, Audio, Video] -> row_order [3,0,1,2]
        // (2 track video, 2 audio), row_of_track [1,2,3,0].
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Audio, TrackKind::Video];
        let row_of_track = [1, 2, 3, 0];
        let row_order = [3, 0, 1, 2];
        // Primaria video (track 0, riga 1) sale di una riga -> track 3
        // (riga 0). Il follower audio (track 1, riga 2) deve scendere di
        // una riga (track 1 -> track 2), non salire: video e audio
        // numerano le track in direzioni opposte.
        let followers = vec![(ClipId(9), 1, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            0,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            2,
            4,
        );
        assert_eq!(
            targets,
            vec![
                (ClipId(1), EffectiveTrack::Existing(3)),
                (ClipId(9), EffectiveTrack::Existing(2)),
            ]
        );
    }

    #[test]
    fn drag_group_row_targets_creates_a_new_track_for_a_follower_that_would_overflow() {
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 3, 1, 0];
        let row_order = [3, 2, 0, 1];
        // Unica track audio: il follower audio non ha dove andare fra
        // quelle esistenti, quindi (bug segnalato) deve chiedere una nuova
        // track anziché restare bloccato sulla propria — esattamente come
        // farebbe se fosse lui la clip afferrata.
        let followers = vec![(ClipId(9), 1, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            2,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            4,
        );
        assert_eq!(targets[1], (ClipId(9), EffectiveTrack::New(1)));
    }

    #[test]
    fn drag_group_row_targets_can_need_more_than_one_new_track_for_a_follower() {
        // La primaria non è la più vicina al bordo del proprio gruppo: se
        // salta direttamente in una nuova track, il follower che era già
        // al bordo deve "sfondare" di più di una track per mantenere la
        // spaziatura relativa.
        let track_kinds = [TrackKind::Video, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 1, 0]; // 3 track video, righe decrescenti
        let row_order = [2, 1, 0];
        // Primaria sulla track 0 (riga 2, la più lontana dal bordo) va in
        // New(1); il follower sulla track 2 (riga 0, già al bordo) segue
        // con lo stesso delta (-3) -> New(3).
        let followers = vec![(ClipId(9), 2, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            0,
            EffectiveTrack::New(1),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            3,
        );
        assert_eq!(targets[1], (ClipId(9), EffectiveTrack::New(3)));
    }

    fn media_clip_visual(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        source_in: FrameIdx,
        source_out: FrameIdx,
        media_id: vv_core::MediaId,
    ) -> ClipVisual<'static> {
        ClipVisual {
            track_index,
            clip: std::borrow::Cow::Owned(Clip::from_source_range(
                ClipId(id),
                ClipSource::Media(media_id),
                source_in,
                source_out,
                start,
                vv_core::Rational::one(),
            )),
            label: String::new(),
            color: egui::Color32::WHITE,
            locked: false,
            muted: false,
        }
    }

    fn project_with_media(duration_frames: FrameIdx) -> (Project, vv_core::MediaId) {
        let mut project = Project::default();
        let media_id = project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/x.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames,
                fps: vv_core::Rational::new(25, 1),
                width: 100,
                height: 100,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        (project, media_id)
    }

    #[test]
    fn drag_set_segments_are_queued_one_after_the_other() {
        let (mut project, a) = project_with_media(30);
        let b = project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/y.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 50,
                fps: vv_core::Rational::new(25, 1),
                width: 100,
                height: 100,
                has_video: true,
                has_audio: true,
                sample_rate: 48000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: None,
        });
        let fps = vv_core::Rational::new(25, 1);
        let set = MediaDragSet {
            items: vec![
                MediaDrag::whole(a, &project.media_pool[a].meta),
                MediaDrag::whole(b, &project.media_pool[b].meta),
            ],
        };
        let segments = drag_set_segments(&project, fps, &TimelineDrag::Media(set));
        assert_eq!(
            segments
                .iter()
                .map(|s| (s.offset, s.len, s.has_audio))
                .collect::<Vec<_>>(),
            vec![(0, 30, false), (30, 50, true)]
        );
    }

    /// Il vicino non limita più il trim: allungandosi sopra di lui lo
    /// sovrascrive (vedi `PendingAction::Trim`), quindi l'unico limite
    /// resta l'inizio del sorgente.
    #[test]
    fn single_trim_range_start_is_not_clamped_by_the_previous_neighbor() {
        let project = Project::default();
        let visuals = vec![
            visual(0, 1, 0, 5), // finisce a 5
            media_clip_visual(0, 2, 10, 8, 20, vv_core::MediaId::default()),
        ];
        let (min_value, _) = single_trim_range(&project, &visuals[1].clip, TrimEdge::Start);
        assert_eq!(min_value, 2, "10 - source_in 8, non il bordo del vicino");
    }

    #[test]
    fn single_trim_range_start_is_clamped_by_source_in() {
        let project = Project::default();
        // Nessun vicino, ma source_in=3: non si può risalire oltre
        // l'inizio del sorgente, quindi timeline_start non può scendere
        // sotto 10-3=7.
        let visuals = vec![media_clip_visual(
            0,
            1,
            10,
            3,
            20,
            vv_core::MediaId::default(),
        )];
        let (min_value, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::Start);
        assert_eq!(min_value, 7);
        assert_eq!(
            max_value, 26,
            "timeline_end() - 1 (timeline_end = 10 + (20-3) = 27)"
        );
    }

    #[test]
    fn single_trim_range_end_is_not_clamped_by_the_next_neighbor() {
        let project = Project::default();
        let visuals = vec![
            visual(0, 1, 0, 10),  // in trim: [0,10)
            visual(0, 2, 15, 10), // vicino successivo: si può sovrascrivere
        ];
        let (_, max_value) = single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, FrameIdx::MAX);
    }

    #[test]
    fn single_trim_range_end_is_clamped_by_media_duration() {
        let (project, media_id) = project_with_media(25);
        // source_out parte da 20 su un media lungo 25 frame: non si può
        // estendere la fine oltre timeline_start + (25 - source_in) = 25.
        let visuals = vec![media_clip_visual(0, 1, 0, 0, 20, media_id)];
        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, 25);
    }

    /// Clip conformata (media a 29,97 su timeline a 30): i limiti di trim
    /// sono in frame di *timeline*, quindi la durata del sorgente va
    /// convertita col `rate` — 1000 frame sorgente sono 1001 di timeline.
    /// Con la conversione 1:1 di prima uscirebbero 1000 e 5000.
    #[test]
    fn single_trim_range_of_a_conformed_clip_is_in_timeline_frames() {
        let (project, media_id) = project_with_media(4000);
        let mut visuals = vec![media_clip_visual(0, 1, 3000, 2000, 2400, media_id)];
        visuals[0].clip = std::borrow::Cow::Owned(Clip::from_source_range(
            visuals[0].clip.id,
            visuals[0].clip.source.clone(),
            2000,
            2400,
            3000,
            vv_core::Rational::conform_rate(
                vv_core::Rational::new(30, 1),
                vv_core::Rational::new(30_000, 1001),
            ),
        ));

        let (min_value, _) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::Start);
        assert_eq!(
            min_value, 998,
            "2000 frame sorgente prima = 2002 di timeline prima di 3000"
        );

        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(
            max_value, 5002,
            "2000 frame sorgente residui = 2002 frame di timeline dopo 3000"
        );
    }

    #[test]
    fn single_trim_range_end_is_unbounded_for_solid_color() {
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10)];
        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, FrameIdx::MAX);
    }

    #[test]
    fn combined_trim_range_intersects_both_clips_constraints() {
        // Video [10,30) generatore (fine illimitata), collegato all'audio
        // [10,30) sulla track 1, il cui media finisce a 25 frame sorgente:
        // il limite della gemella vale anche per il video.
        let (project, media_id) = project_with_media(25);
        let group = Some(vv_core::LinkGroupId(9));
        let mut video = visual(0, 1, 10, 20);
        video.clip.to_mut().linked_group = group;
        let mut audio = media_clip_visual(1, 2, 10, 0, 20, media_id);
        audio.clip.to_mut().linked_group = group;
        let visuals = vec![video, audio];

        let (_, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((1, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(
            max_value, 35,
            "vincolo della gemella si applica anche al video"
        );
        assert_eq!(followers, vec![(ClipId(2), 1, 0, TrimEdge::End)]);
    }

    #[test]
    fn combined_trim_range_shifts_each_selected_clip_by_its_offset() {
        // Fine della primaria a 10, dell'altra (track 1) a 25: stesso
        // delta per entrambe, e l'altra non può scendere sotto 21.
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 20, 5)];
        let (min_value, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((1, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(followers, vec![(ClipId(2), 1, 15, TrimEdge::End)]);
        assert_eq!(min_value, 6, "21 - 15");
        assert_eq!(max_value, FrameIdx::MAX - 15);
    }

    #[test]
    fn combined_trim_range_stops_before_another_trimmed_clip_on_the_same_track() {
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 5)];
        let (_, max_value, _) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((0, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(max_value, 20);
    }

    #[test]
    fn combined_trim_range_rolls_between_two_adjacent_clips() {
        // [0,10) e [10,25) a contatto, la seconda con 5 frame di sorgente
        // prima del suo inizio: il punto di contatto va da 5 a 24 (la
        // seconda resta lunga almeno 1).
        let project = Project::default();
        let mut second = visual(0, 2, 10, 15);
        second.clip = std::borrow::Cow::Owned(Clip::from_source_range(
            ClipId(2),
            vv_core::ClipSource::SolidColor,
            5,
            20,
            10,
            vv_core::Rational::one(),
        ));
        let visuals = vec![visual(0, 1, 0, 10), second];
        let (min_value, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(2)),
            TrimEdge::Start,
            &[((0, ClipId(1)), TrimEdge::End)],
        );
        assert_eq!((min_value, max_value), (5, 24));
        assert_eq!(followers, vec![(ClipId(1), 0, 0, TrimEdge::End)]);
    }

    #[test]
    fn edge_zones_roll_at_the_contact_point_and_trim_just_inside() {
        let neighbor = Some((0, ClipId(9)));
        let zones = edge_zones(100.0, None, neighbor);
        assert_eq!(zones.at(2.0), Some(EdgeZone::Trim(TrimEdge::Start)));
        assert_eq!(zones.at(50.0), None);
        assert_eq!(zones.at(90.0), Some(EdgeZone::Trim(TrimEdge::End)));
        assert_eq!(
            zones.at(98.0),
            Some(EdgeZone::Roll { edge: TrimEdge::End, neighbor: (0, ClipId(9)) })
        );
    }

    /// Bug segnalato: con la calamita il bordo si fermava un frame prima
    /// o dopo la testina, che non era un punto di aggancio.
    #[test]
    fn snap_frame_of_an_edge_snaps_to_the_playhead() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_frame(14, 0, &visuals, &[ClipId(1)], &[15], 5.0, true), 15);
        assert_eq!(snap_frame(4, 10, &visuals, &[ClipId(1)], &[15], 5.0, true), 5);
    }

    #[test]
    fn snap_frame_of_an_edge_snaps_the_trimmed_edge_to_a_nearby_clip_edge() {
        // Clip vicina [20,30): il bordo trascinato a 18, entro soglia
        // (10px / 5px per frame = 2 frame), si aggancia a 20.
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_frame(18, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 20);
    }

    #[test]
    fn snap_frame_of_an_edge_ignores_the_clip_being_trimmed_and_far_edges() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        // Il proprio bordo (10) non è un aggancio valido.
        assert_eq!(snap_frame(11, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 11);
        // Fuori soglia: nessun aggancio.
        assert_eq!(snap_frame(15, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 15);
        // Calamita spenta: nessun aggancio nemmeno entro soglia.
        assert_eq!(snap_frame(18, 0, &visuals, &[ClipId(1)], &[], 5.0, false), 18);
    }

    #[test]
    fn snap_frame_snaps_start_to_nearby_clip_end() {
        // Clip esistente [0,10): il suo bordo di fine è 10. Un candidato a
        // 12 (entro soglia) deve agganciarsi esattamente lì.
        let visuals = vec![visual(0, 1, 0, 10)];
        let px_per_frame = 5.0; // soglia 10px / 5px_per_frame = 2 frame
        let snapped = snap_frame(12, 20, &visuals, &[], &[], px_per_frame, true);
        assert_eq!(snapped, 10);
    }

    #[test]
    fn snap_frame_snaps_end_of_dragged_clip_to_nearby_clip_start() {
        // Clip esistente [50,60): la clip trascinata (lunga 20) deve
        // agganciare la propria *fine* a 50, cioè candidate_start=30.
        let visuals = vec![visual(0, 1, 50, 10)];
        let snapped = snap_frame(32, 20, &visuals, &[], &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_ignores_clips_beyond_threshold() {
        let visuals = vec![visual(0, 1, 0, 10)];
        // 20 frame di distanza dal bordo (10): a px_per_frame=5.0 la soglia
        // è di soli 2 frame, quindi resta invariato.
        let snapped = snap_frame(30, 5, &visuals, &[], &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_disabled_is_a_no_op() {
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[], &[], 5.0, false);
        assert_eq!(snapped, 12);
    }

    #[test]
    fn snap_frame_excludes_given_clip_ids() {
        // La clip 1 sarebbe un aggancio valido, ma è esclusa (è la clip
        // stessa che si sta trascinando, o la sua gemella collegata).
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[ClipId(1)], &[], 5.0, true);
        assert_eq!(snapped, 12);
    }

    #[test]
    fn duplicate_clips_keeps_the_originals_relinks_the_copies_and_cuts_what_they_cover() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        let solid = |id, start, len| {
            Clip::from_source_range(
                id,
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            )
        };
        // Video [0,20) collegato all'audio [0,20); più avanti sul video
        // un'altra clip [30,60) che la copia coprirà in parte.
        let (v, a, other) = (project.alloc_clip_id(), project.alloc_clip_id(), project.alloc_clip_id());
        for (track_index, clip) in [(0, solid(v, 0, 20)), (1, solid(a, 0, 20)), (0, solid(other, 30, 30))] {
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip { timeline: timeline_id, track_index, clip }),
            );
        }
        history.do_command(
            &mut project,
            Box::new(vv_core::LinkClips::new(timeline_id, vec![(0, v), (1, a)])),
        );

        let mut state = TimelineState::default();
        duplicate_clips(
            &mut project,
            &mut history,
            &mut state,
            timeline_id,
            &[(v, 0, 0, 25), (a, 1, 1, 25)],
        );

        let tl = &project.timelines[timeline_id];
        let spans = |track: usize| -> Vec<(FrameIdx, FrameIdx)> {
            tl.tracks[track].clips.iter().map(|c| (c.timeline_start, c.timeline_end())).collect()
        };
        assert_eq!(spans(0), vec![(0, 20), (25, 45), (45, 60)], "l'altra clip viene tagliata");
        assert_eq!(spans(1), vec![(0, 20), (25, 45)]);
        let copy_v = &tl.tracks[0].clips[1];
        let copy_a = &tl.tracks[1].clips[1];
        assert!(copy_v.linked_group.is_some());
        assert_eq!(copy_v.linked_group, copy_a.linked_group);
        assert_ne!(copy_v.linked_group, tl.tracks[0].clips[0].linked_group);
        assert_eq!(
            state.selected,
            BTreeSet::from([(0, copy_v.id), (1, copy_a.id)])
        );

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2, "un solo passo di undo");
    }

    /// Esegue `show_timeline` per davvero dentro un `egui::Context`
    /// headless, con clip vere su più track: intercetta panic/bug nel
    /// codice di disegno (indici, borrow) che i test puramente logici
    /// sopra non toccano.
    #[test]
    fn show_timeline_renders_without_panicking_with_real_clips() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();

        for (track_index, start, len) in [(0usize, 0i64, 50i64), (0, 50, 30), (1, 0, 50)] {
            let clip = Clip::from_source_range(
                project.alloc_clip_id(),
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            );
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip {
                    timeline: timeline_id,
                    track_index,
                    clip,
                }),
            );
        }

        let mut state = TimelineState {
            selected: BTreeSet::from([(0, ClipId(0))]),
            ..TimelineState::default()
        };

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_timeline(
                    ui,
                    &mut project,
                    &mut history,
                    timeline_id,
                    &|_id| "media".to_string(),
                    &mut state,
                    true,
                    true,
                    &[],
                    &[],
                    &std::collections::HashMap::new(),
                    false,
                );
            });
        });
        // Il font atlas genera una texture delta: va consumata esplicitamente
        // o egui panica al drop (diagnostica pensata per un vero renderer).
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);
    }

    /// Esegue `show_timeline` con più di una track video (REFACTOR_PIPELINE.md
    /// B4): la colonna di header (etichette + pulsanti aggiungi/rimuovi
    /// track, `draw_track_headers`) deve reggere N track qualunque, non
    /// solo la coppia fissa video/audio di prima.
    #[test]
    fn show_timeline_renders_without_panicking_with_more_than_two_tracks() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        let mut state = TimelineState::default();

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_timeline(
                    ui,
                    &mut project,
                    &mut history,
                    timeline_id,
                    &|_id| "media".to_string(),
                    &mut state,
                    true,
                    true,
                    &[],
                    &[],
                    &std::collections::HashMap::new(),
                    false,
                );
            });
        });
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks.len(), 3);
    }

    /// Bug: lo zoom "cresceva" dal bordo sinistro *visibile* (o da 0) invece
    /// che dalla testina. Correzione: quando `pixels_per_sec` cambia,
    /// `show_timeline` corregge l'offset di scroll orizzontale così la
    /// testina resta alla stessa posizione a schermo — `offset' = offset +
    /// t_playhead * (pps' - pps)` — e uno zoom in/out ripetuto non la sposta.
    #[test]
    fn zoom_keeps_playhead_at_same_screen_position() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        // Clip lunga abbastanza da rendere la timeline scrollabile (contenuto
        // più largo del viewport): senza, l'offset verrebbe clampato a 0 e il
        // test non direbbe nulla.
        let clip = Clip::from_source_range(
            project.alloc_clip_id(),
            vv_core::ClipSource::SolidColor,
            0,
            2500, // 100s a 25fps
            0,
            vv_core::Rational::one(),
        );
        history.do_command(
            &mut project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );

        let mut state = TimelineState::default();
        state.playhead = 125; // t = 5s a 25fps

        let ctx = egui::Context::default();
        // Viewport realistico (800x600): con `RawInput::default()` il
        // viewport headless è enorme (10000x10000) e il contenuto non
        // sarebbe scrollabile — l'offset verrebbe clampato a 0 e il test
        // non direbbe nulla.
        let frame_input = || {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(egui::Rect::from_min_max(
                egui::pos2(0.0, 0.0),
                egui::pos2(800.0, 600.0),
            ));
            input
        };
        let mut render_frame = |state: &mut TimelineState| {
            let mut output = ctx.run_ui(frame_input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    show_timeline(
                        ui,
                        &mut project,
                        &mut history,
                        timeline_id,
                        &|_id| "media".to_string(),
                        state,
                        true,
                        true,
                        &[],
                        &[],
                        &std::collections::HashMap::new(),
                        false,
                    );
                });
            });
            output.textures_delta.clear();
        };

        // Frame 1: inizializza lo stato della ScrollArea.
        render_frame(&mut state);
        let scroll_id = {
            // Stesso id che `show_timeline` usa per la sua ScrollArea:
            // `make_persistent_id` usa l'id *stabile* dell'Ui (non il
            // contatore auto-id), quindi basta replicare la stessa
            // struttura di nesting — CentralPanel -> `horizontal_top`,
            // dove dentro `show_timeline` vive la ScrollArea. Attenzione a
            // usare la forma `IdSalt` e non la stringa: `Id::with(IdSalt)` e
            // `Id::with(&str)` danno id diversi per la stessa stringa.
            let mut captured = None;
            let mut output = ctx.run_ui(frame_input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        captured = Some(ui.make_persistent_id(egui::IdSalt::new("timeline_scroll")));
                    });
                });
            });
            output.textures_delta.clear();
            captured.expect("id della ScrollArea")
        };

        // Simula l'utente che ha già scrollato: offset 30px.
        {
            let mut st = egui::containers::scroll_area::State::load(&ctx, scroll_id)
                .expect("stato ScrollArea dopo frame 1");
            assert_eq!(st.offset.x, 0.0);
            st.offset.x = 30.0;
            st.store(&ctx, scroll_id);
        }

        // Zoom in: pps 60 -> 75. La testina (t=5s) deve restare alla stessa
        // posizione a schermo: offset' = 30 + 5 * (75 - 60) = 105.
        state.zoom_in();
        render_frame(&mut state);
        let st = egui::containers::scroll_area::State::load(&ctx, scroll_id).unwrap();
        assert_eq!(st.offset.x, 105.0);

        // Zoom out: pps 75 -> 60. La testina non si è mossa, quindi l'offset
        // torna esattamente a 30.
        state.zoom_out();
        render_frame(&mut state);
        let st = egui::containers::scroll_area::State::load(&ctx, scroll_id).unwrap();
        assert_eq!(st.offset.x, 30.0);
    }

    /// Bug: il pannello timeline (`Panel::bottom` con dentro
    /// `show_timeline`, vedi `main.rs`) tornava alla dimensione del
    /// contenuto invece di restare a quella a cui l'utente l'aveva
    /// ridimensionato, non appena passava un frame senza interazione.
    /// Causa: `ScrollArea` di default si restringe al contenuto invece
    /// di riempire lo spazio assegnato dal `Panel` (`auto_shrink` è
    /// `true` su entrambi gli assi di default). Con poche clip corte
    /// (contenuto reale molto più basso di 240px) il pannello, su più
    /// frame consecutivi senza alcuna interazione, non deve restringersi
    /// sotto la dimensione richiesta.
    #[test]
    fn show_timeline_panel_does_not_shrink_to_short_content() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![vv_core::Track::new(TrackKind::Video)],
        });
        let mut history = History::default();
        let clip = Clip::from_source_range(
            project.alloc_clip_id(),
            vv_core::ClipSource::SolidColor,
            0,
            10,
            0,
            vv_core::Rational::one(),
        );
        history.do_command(
            &mut project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );
        let mut state = TimelineState::default();

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let mut last_height = 0.0_f32;
        for _ in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let panel_resp = egui::Panel::bottom("timeline_repro")
                    .default_size(240.0)
                    .resizable(true)
                    .show(ui, |ui| {
                        show_timeline(
                            ui,
                            &mut project,
                            &mut history,
                            timeline_id,
                            &|_id| "media".to_string(),
                            &mut state,
                            true,
                            true,
                            &[],
                            &[],
                            &std::collections::HashMap::new(),
                            false,
                        );
                    });
                last_height = panel_resp.response.rect.height();
            });
            output.textures_delta.clear();
        }
        assert!(
            last_height > 200.0,
            "il pannello si è ristretto al contenuto ({last_height}px) invece di restare vicino ai 240px richiesti"
        );
    }
}
