//! Widget timeline multi-traccia: disegna tracce/clip, gestisce
//! multi-selezione (click semplice, ctrl+click per aggiungere/togglere,
//! shift+click per un range, rettangolo di selezione trascinando da
//! un'area vuota — ognuna espande sempre alla chiusura dei gruppi
//! collegati, vedi `expand_to_linked_groups`), drag orizzontale (l'intera
//! selezione si muove insieme, vedi `drag_group_for`/`Clip::linked_group`),
//! menu contestuale per collegare/scollegare, e il playhead. Ogni mutazione
//! del progetto passa da
//! `History::do_command`, mai da una modifica diretta del `Project`; i
//! cambi di sola selezione invece mutano `TimelineState` direttamente,
//! visto che non toccano `project`/`history`.
//!
//! Disegno "immediate mode" a basso livello (painter diretto, non widget
//! egui nidificati): per una griglia densa di rettangoli come una timeline
//! dà più controllo e meno overhead dei container annidati.

use std::collections::BTreeSet;

use vv_core::{
    Clip, ClipId, ClipSource, FrameIdx, History, Keyframed, Project, TimelineId, Track,
    TrackKind, TrimEdge,
};

const ROW_HEIGHT: f32 = 40.0;
const RULER_HEIGHT: f32 = 20.0;
const MIN_TIMELINE_SECS: f64 = 20.0;
const TRAILING_MARGIN_SECS: f64 = 5.0;
/// Altezza del separatore trascinabile fra il gruppo Video e il gruppo Audio.
const GROUP_DIVIDER_HEIGHT: f32 = 8.0;
/// Colonna fissa a sinistra della timeline (etichetta track + rimuovi),
/// non coinvolta nello scroll orizzontale — vedi `draw_track_headers`.
const TRACK_HEADER_WIDTH: f32 = 100.0;

/// (indice track, id clip): coppia usata ovunque per identificare univocamente
/// una clip nella timeline (l'id da solo non basta, la stessa clip non può
/// stare su più track ma l'id è comunque solo un contatore globale, non scoped
/// per track).
type ClipKey = (usize, ClipId);

pub struct TimelineState {
    /// Clip selezionate. Vuoto se nessuna clip è selezionata (non deve
    /// essere confuso con "nessuna timeline": qui è solo lo stato della
    /// selezione dentro una timeline esistente).
    pub selected: BTreeSet<ClipKey>,
    /// Ultima clip toccata da un click semplice o da un ctrl+click: origine
    /// del range per il prossimo shift+click. Uno shift+click *non* sposta
    /// l'ancora, così shift+click ripetuti restano relativi alla stessa
    /// origine (come nei file manager).
    selection_anchor: Option<ClipKey>,
    pub playhead: FrameIdx,
    pixels_per_sec: f32,
    /// `pixels_per_sec` usato nell'ultimo passaggio di disegno della
    /// timeline: il confronto col valore corrente in `show_timeline`
    /// rileva lo zoom avvenuto *in questo frame* (da tastiera/menu — che
    /// mutano `pixels_per_sec` prima del disegno — o da Alt+scroll) e
    /// permette di correggere l'offset di scroll per ancorare lo zoom alla
    /// testina.
    last_rendered_pps: f32,
    drag: Option<DragState>,
    /// Rettangolo di selezione in corso, in coordinate locali al contenuto
    /// scrollabile (senza l'offset di `origin`, così resta valido anche se
    /// lo scroll cambia): un drag avviato da un'area vuota della timeline
    /// (non su una clip, non sul righello) lo popola.
    marquee: Option<MarqueeDrag>,
    /// Vuoto selezionato con un click su uno spazio vuoto *preceduto* da
    /// una clip successiva sulla stessa track — (track_index, inizio,
    /// fine), nello spazio frame della timeline. Mutuamente esclusivo con
    /// `selected` (selezionare l'uno svuota l'altro): serve a dare a un
    /// vuoto un'identità cliccabile/cancellabile con ripple delete, come in
    /// DaVinci Resolve. Uno spazio vuoto in coda (nessuna clip dopo) non è
    /// un "vuoto" selezionabile: non c'è nulla da ravvicinare shiftandolo.
    pub selected_gap: Option<(usize, FrameIdx, FrameIdx)>,
    /// Clip copiate (Ctrl+C in `main.rs`), pronte per essere incollate
    /// (Ctrl+V) alla posizione del playhead. Vuoto se non è ancora mai
    /// stato copiato nulla in questa sessione.
    pub clipboard: Vec<ClipboardEntry>,
    trim: Option<TrimState>,
    /// Margine sopra al gruppo Video se l'utente ha trascinato il
    /// separatore (vedi `GROUP_DIVIDER_HEIGHT`); `None` = centrato di default.
    track_top_margin: Option<f32>,
    /// In/out della timeline: porzione esportata.
    pub export_marks: crate::transport::MarkRange,
}

/// Una clip copiata: né l'id né il `timeline_start` assoluto sopravvivono
/// al copia/incolla — l'id va riallocato al paste (i vecchi potrebbero non
/// esistere più, o esistere ma riferirsi a un'altra clip), e la posizione
/// è relativa all'inizio più a sinistra tra le clip copiate insieme (così
/// incollare un gruppo ne preserva la disposizione relativa, ancorata al
/// playhead al momento dell'incolla).
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
    /// Tag locale all'operazione di copia (non un vero `LinkGroupId`, che
    /// va riallocato al paste): entry con lo stesso tag `Some(_)` erano nel
    /// gruppo collegato al momento della copia — permette di ricollegare
    /// le nuove clip incollate tra loro, dato che i `ClipId`/`LinkGroupId`
    /// originali non si riportano al paste.
    pub link_tag: Option<u64>,
}

struct MarqueeDrag {
    start: egui::Pos2,
    current: egui::Pos2,
}

struct DragState {
    /// La clip su cui l'utente ha effettivamente premuto per iniziare il
    /// drag: la sua posizione pilota lo snap (vedi
    /// `dragged_primary_new_start`), le altre in `followers` la seguono
    /// mantenendo l'offset relativo catturato a inizio drag.
    clip_id: ClipId,
    /// Track di partenza: la track candidata (vedi `track_drag_target`)
    /// può differire durante il drag, i bound si ricalcolano ogni frame.
    track_index: usize,
    original_start: FrameIdx,
    accum_px: f32,
    /// (clip_id, track_index, offset) di ogni altra clip che si muove
    /// insieme alla primaria in questo drag — l'intera selezione corrente
    /// al momento in cui il drag è iniziato (che contiene sempre un intero
    /// gruppo collegato, mai una parte, vedi doc del modulo) più il gruppo
    /// della clip cliccata se non era già selezionata. `offset` è la
    /// distanza fissa `quella.timeline_start - primaria.timeline_start`
    /// catturata all'inizio del drag.
    followers: Vec<(ClipId, usize, FrameIdx)>,
    /// Drag iniziato con ALT: al rilascio si inseriscono delle copie, gli
    /// originali restano dove sono.
    duplicate: bool,
}

/// Trim di un bordo, tenuto separato da `DragState` (mossa vera e propria)
/// invece di forzarlo nello stesso stato: le due interazioni sono comunque
/// mutuamente esclusive (un drag comincia o come Move o come trim, mai
/// entrambi), ma tenerle distinte evita di dover sovraccaricare i campi di
/// `DragState` con significati diversi a seconda del `kind`.
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
    /// (clip_id, track_index, offset, bordo) delle altre clip trimmate
    /// insieme (selezione o gruppo collegato, come per
    /// `DragState::followers`; nel roll anche la vicina, col bordo
    /// opposto): ogni bordo si sposta dello stesso delta, `offset` è la
    /// distanza fra il loro bordo e quello della primaria.
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

/// Limiti di zoom orizzontale della timeline (`pixels_per_sec`).
/// Estremo minimo di zoom: abbastanza basso da poter vedere per intero
/// almeno un'ora di contenuto (3600s) anche in un pannello timeline non
/// enorme — a 0.1px/sec, un'ora sta in 360px.
const MIN_PIXELS_PER_SEC: f32 = 0.1;
const MAX_PIXELS_PER_SEC: f32 = 800.0;

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
            clipboard: Vec::new(),
            trim: None,
            track_top_margin: None,
            export_marks: crate::transport::MarkRange::default(),
        }
    }
}

impl TimelineState {
    /// Imposta selezione e ancora esplicitamente: usato da `main.rs`
    /// quando deve costruire una selezione (anche multipla) derivata da un
    /// comando — es. "selection follows playhead" dopo un taglio, che
    /// seleziona la clip video appena tagliata *e* la sua gemella audio
    /// collegata — non da un'interazione diretta con la timeline (quella
    /// passa dai rami `Click`/marquee dentro `show_timeline`).
    pub fn set_selection(&mut self, selected: BTreeSet<ClipKey>, anchor: Option<ClipKey>) {
        self.selected = selected;
        self.selection_anchor = anchor;
        self.selected_gap = None;
    }

    /// Imposta la selezione a una singola clip (o a nessuna), aggiornando
    /// anche l'ancora di conseguenza: usato da "selection follows
    /// playhead" (in `main.rs`), che deve sempre collassare a una clip
    /// sola anche se la selezione precedente era multipla.
    pub fn set_single_selection(&mut self, clip: Option<ClipKey>) {
        self.set_selection(clip.into_iter().collect(), clip);
    }

    /// Svuota la selezione (clip e vuoto).
    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.selection_anchor = None;
        self.selected_gap = None;
    }

    /// Zoom orizzontale della timeline (moltiplica `pixels_per_sec` per un
    /// fattore fisso a ogni passo): usato dalla shortcut da tastiera in
    /// `main.rs`, oltre a Alt+scroll/pinch gestito dentro `show_timeline`.
    /// Lo zoom è ancorato alla testina: `show_timeline` corregge l'offset
    /// di scroll orizzontale così la testina resta alla stessa posizione a
    /// schermo (non "cresce" dal bordo sinistro visibile).
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

struct ClipVisual {
    track_index: usize,
    clip: Clip,
    label: String,
    color: egui::Color32,
}

/// Comando differito: raccolto durante il disegno (che prende in prestito
/// `project` immutabilmente) e applicato subito dopo, per evitare un
/// conflitto di borrow con `history.do_command(project, ...)`. I cambi di
/// sola selezione non passano di qui: mutano `state` direttamente, dato
/// che non serve né `project` né `history`.
enum PendingAction {
    /// Sposta un intero gruppo di clip trascinato (vedi `show_timeline`,
    /// `drag_group_row_targets`). `new_video_tracks`/`new_audio_tracks`
    /// vanno create *prima* di risolvere le eventuali `TrackDestination::New`
    /// in `moves` (una per clip del gruppo: clip_id, from_track,
    /// destinazione, new_start) — più di una clip può richiedere una nuova
    /// track dello stesso tipo, a `depth` diverse (vedi `EffectiveTrack`).
    Move {
        new_video_tracks: usize,
        new_audio_tracks: usize,
        moves: Vec<(ClipId, usize, TrackDestination, FrameIdx)>,
        duplicate: bool,
    },
    /// (clip_id, track_index, edge, nuova posizione del bordo) per ogni
    /// clip del gruppo collegato, più il tratto di timeline che ciascuna
    /// si è appena presa allungandosi (`(track_index, start, end)`): quel
    /// che c'era lì viene sovrascritto, come in un vero NLE.
    Trim {
        trims: Vec<(ClipId, usize, TrimEdge, FrameIdx)>,
        overwritten: Vec<(usize, FrameIdx, FrameIdx)>,
    },
    Unlink(usize, ClipId),
    /// Collega tutte le clip elencate (track_index, clip_id) in un unico
    /// gruppo nuovo — almeno 2.
    Link(Vec<ClipKey>),
    /// Rimuove la track a questo indice (e le sue clip).
    RemoveTrack(usize),
}

/// Ordine di disegno: gruppo Video (decrescente per `track_index` — la
/// track appena aggiunta trascinando sopra finisce in cima, come V2 sopra
/// V1 in un NLE) seguito dal gruppo Audio (crescente, dove appendere basta
/// già a mettere la nuova track in fondo). Indipendente da come le track
/// sono intercalate in `Track::tracks`, che conta solo per il compositing.
fn track_row_order(track_kinds: &[TrackKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..track_kinds.len())
        .filter(|&i| track_kinds[i] == TrackKind::Video)
        .collect();
    order.reverse();
    order.extend((0..track_kinds.len()).filter(|&i| track_kinds[i] == TrackKind::Audio));
    order
}

enum TrackDragTarget {
    Track(usize),
    NewTrack,
}

/// Track candidata per un drag in corso: `Existing` è sempre risolta a un
/// `track_index` reale (la track di partenza se il puntatore non è in
/// nessuna zona valida, vedi `track_drag_target`); `New(depth)` significa
/// che va creata al rilascio, `depth` (1-based) conta quante nuove track
/// dello stesso tipo servono prima di questa — un follower può aver
/// bisogno di più di una nuova track se, per mantenere la spaziatura
/// relativa del gruppo, deve andare oltre quella su cui atterra la
/// primaria (sempre a `depth` 1, vedi `drag_group_row_targets`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveTrack {
    Existing(usize),
    New(usize),
}

/// Come `EffectiveTrack`, ma con il `TrackKind` portato esplicitamente:
/// serve a `PendingAction::Move` per sapere quale contatore
/// (`new_video_tracks`/`new_audio_tracks`) usare per un target `New`.
#[derive(Clone, Copy)]
enum TrackDestination {
    Existing(usize),
    New(TrackKind, usize),
}

/// Dove atterrerebbe una clip di tipo `kind` trascinata a `local_y`: su una
/// track esistente, o in una nuova track se `local_y` cade nel margine
/// sopra al gruppo Video/sotto al gruppo Audio (le zone di drop in
/// `show_timeline`). `None` se non è in nessuna zona valida per `kind`
/// (gruppo dell'altro tipo, separatore) — il chiamante resta sulla track
/// di partenza.
fn track_drag_target(
    local_y: f32,
    kind: TrackKind,
    row_order: &[usize],
    video_count: usize,
    divider_height: f32,
    top_margin: f32,
    bottom_margin: f32,
    rows_height: f32,
) -> Option<TrackDragTarget> {
    let y_in_rows = local_y - RULER_HEIGHT - top_margin;
    let video_rows_height = video_count as f32 * ROW_HEIGHT;
    match kind {
        TrackKind::Video => {
            if y_in_rows < 0.0 {
                return (top_margin > 0.0).then_some(TrackDragTarget::NewTrack);
            }
            if y_in_rows < video_rows_height {
                let row = (y_in_rows / ROW_HEIGHT) as usize;
                return row_order.get(row).copied().map(TrackDragTarget::Track);
            }
            None
        }
        TrackKind::Audio => {
            let audio_start = video_rows_height + divider_height;
            if y_in_rows >= audio_start && y_in_rows < rows_height {
                let row = ((y_in_rows - audio_start) / ROW_HEIGHT) as usize;
                return row_order.get(video_count + row).copied().map(TrackDragTarget::Track);
            }
            if y_in_rows >= rows_height && y_in_rows < rows_height + bottom_margin {
                return Some(TrackDragTarget::NewTrack);
            }
            None
        }
    }
}

/// Track target di ogni clip di un gruppo in trascinamento verticale
/// (primaria in testa, poi un elemento per ogni `followers`, stesso
/// ordine): la primaria atterra su `primary_target` (già risolto da
/// `track_drag_target`), i follower si spostano dello stesso numero di
/// righe — invertito se di tipo diverso dalla primaria, dato che video e
/// audio numerano le track in direzioni opposte (vedi `track_row_order`).
/// Un follower può finire oltre l'ultima track esistente del proprio tipo
/// (per mantenere la spaziatura relativa del gruppo, se la primaria non
/// era la più vicina al bordo): in quel caso il target è `New(depth)`
/// invece di essere bloccato all'ultima track, esattamente come farebbe
/// da sé trascinato singolarmente fin lì. Nella direzione opposta (dove
/// non esiste alcuna zona "nuova track", vedi `track_drag_target`) resta
/// invece bloccato alla track più vicina.
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

/// Colonna fissa a sinistra: etichetta e "×" di ogni track, alle stesse
/// `row_y` del contenuto scrollabile, più il timestamp della testina.
/// Disegnata a mano (painter + `ui.interact`, non widget in flow): un
/// `ui.add_space` legato all'altezza del pannello qui dentro farebbe
/// crescere `Panel::bottom` all'infinito (vedi bug "il pannello prende
/// tutto lo spazio all'apertura").
fn draw_track_headers(
    ui: &mut egui::Ui,
    track_kinds: &[TrackKind],
    row_order: &[usize],
    row_y: &[f32],
    video_count: usize,
    divider_height: f32,
    top_margin: f32,
    slack: f32,
    track_top_margin: &mut Option<f32>,
    natural_content_height: f32,
    pending: &mut Option<PendingAction>,
    playhead: FrameIdx,
    fps: f64,
) {
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(TRACK_HEADER_WIDTH, natural_content_height.max(RULER_HEIGHT)),
        egui::Sense::hover(),
    );
    let origin = rect.min;
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
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + row_y[track_index]),
            egui::vec2(TRACK_HEADER_WIDTH, ROW_HEIGHT),
        );
        let number = track_kinds[..=track_index].iter().filter(|k| **k == kind).count();
        let label = match kind {
            TrackKind::Video => format!("V{number}"),
            TrackKind::Audio => format!("A{number}"),
        };
        ui.painter().text(
            row_rect.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            text_color,
        );

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
                "Non si può rimuovere l'ultima track di questo tipo"
            } else {
                "Rimuovi track"
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

    // Stesso separatore dell'area scrollabile (stato condiviso in
    // `track_top_margin`), trascinabile anche da qui.
    if divider_height > 0.0 && video_count < row_order.len() {
        let audio_first_track = row_order[video_count];
        let divider_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + row_y[audio_first_track] - divider_height),
            egui::vec2(TRACK_HEADER_WIDTH, divider_height),
        );
        let divider_resp = ui.interact(
            divider_rect,
            ui.id().with("timeline_header_track_split"),
            egui::Sense::drag(),
        );
        let active = divider_resp.hovered() || divider_resp.dragged();
        if active {
            ui.ctx()
                .output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
        }
        if divider_resp.dragged() {
            *track_top_margin =
                Some((top_margin + divider_resp.drag_delta().y).clamp(0.0, slack));
        }
        ui.painter().hline(
            divider_rect.x_range(),
            divider_rect.center().y,
            egui::Stroke::new(
                1.0,
                egui::Color32::from_gray(if active { 160 } else { 80 }),
            ),
        );
    }

}

/// Intervallo (in secondi) tra due tacche *maggiori* del righello, scelto
/// dalla sequenza "1-2-5" (stessa usata dai grafici per assi leggibili) —
/// il primo valore che tiene le tacche ad almeno `MIN_MAJOR_TICK_PX`
/// l'una dall'altra alla scala corrente. Necessario perché
/// `pixels_per_sec` copre un range enorme (0.1-800, vedi
/// `MIN_PIXELS_PER_SEC`/`MAX_PIXELS_PER_SEC`): un intervallo fisso
/// sarebbe illeggibile a un estremo o inutilmente denso all'altro.
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

/// Timecode HH:MM:SS:FRAME per le etichette del righello. Il frame è
/// calcolato dalla frazione di secondo residua moltiplicata per l'fps della
/// timeline (arrotondato all'intero più vicino, clampato a [0, fps-1]).
fn format_timecode(total_secs: f64, fps: f64) -> String {
    let total_secs = total_secs.max(0.0);
    let frame_f = (total_secs * fps).round() as i64;
    let h = frame_f / (fps as i64 * 3600);
    let m = (frame_f / (fps as i64 * 60)) % 60;
    let s = (frame_f / (fps as i64)) % 60;
    let f = frame_f % (fps as i64);
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

/// Sistema di tacche a 3 altezze che si adatta allo zoom:
/// - Livello 0 (corto): ogni frame — visibile solo quando lo zoom è alto
///   abbastanza da non farle toccare.
/// - Livello 1 (medio): ogni N frames (N calcolato dinamicamente) — appare
///   quando il livello 0 diventa troppo denso, scompare quando anche lui
///   diventerebbe illeggibile.
/// - Livello 2 (alto + etichetta HH:MM:SS:FF): intervallo adattivo "pulito"
///   (sequenza 1-2-5) — sempre visibile, è il riferimento temporale.
///
/// Le soglie di visibilità sono in pixel: se due tacche successive dello
/// stesso livello sarebbero più vicine di `MIN_TICK_SPACING_PX`, quel
/// livello non si disegna (sarebbe solo un addensamento illeggibile).
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

    // Livello 1: tacche medie — intervalli "puliti" tra i frame, ad esempio
    // ogni 5 o 10 frames a seconda dello zoom (calcolato dinamicamente).
    // Visibili solo se non si sovrappongono alle maggiori e non sono troppo
    // vicine tra loro.
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
}

impl MediaDrag {
    pub fn whole(media_id: vv_core::MediaId, meta: &vv_core::MediaMeta) -> Self {
        Self {
            media_id,
            source_in: 0,
            source_out: meta.duration_frames,
        }
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

    pub fn label(self) -> &'static str {
        match self {
            Generator::SolidColor => "Solid Color",
            Generator::Text => "Text",
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
        // va scelto il tipo giusto prima di prenderlo.
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

/// Un segmento del ghost di drop: offset dall'inizio del drop, lunghezza e
/// quali stream porta il media. I media vengono accodati uno dopo l'altro
/// (vedi `insert_media_clip`), quindi il ghost li mostra separati e non come
/// un unico blocco lungo quanto la somma.
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
                has_video: project.media_pool[d.media_id].meta.has_video,
                has_audio: project.media_pool[d.media_id].meta.has_audio,
            };
            offset += len;
            seg
        })
        .collect()
}

/// Dove piazzare un media rilasciato dal media pool. `Default`: track di
/// sempre (vedi `insert_media_clip`). `NewVideoTrack`/`NewAudioTrack`:
/// rilasciato nella fascia vuota sopra al gruppo Video o sotto al gruppo
/// Audio, crea al volo quella track e la usa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDropTarget {
    Default,
    NewVideoTrack,
    NewAudioTrack,
    /// Track video esistente sotto al puntatore (drop di un effetto).
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
    // Intervalli (in frame di timeline) già decodificati/in cache in
    // `RenderAhead` — disegnati come una sottile
    // striscia "buffered" nel righello (richiesta: "visualizzare durante
    // la riproduzione come viene fatto il buffer"), stesso principio dei
    // player video comuni (es. la barra grigia sotto la barra di
    // avanzamento di YouTube).
    buffered_ranges: &[(FrameIdx, FrameIdx)],
    // Intervalli (in frame di timeline) delle clip attualmente servite
    // dal proxy invece che dal sorgente (REFACTOR_PIPELINE.md proxy) —
    // disegnati come una striscia sulla clip stessa, non sul righello:
    // il proxy è una proprietà *per clip* (o meglio: per intera clip, si
    // veda `VibeVideoApp::proxy_timeline_ranges`), non di un punto della
    // timeline come invece lo è "buffered" — indica *da dove*
    // arriverebbe il frame quando viene bufferizzato, non se è già
    // pronto ora (le due cose sono indipendenti: una clip proxy-backed
    // può avere solo una parte già in cache, o viceversa una clip non
    // proxy-backed può comunque essere già bufferizzata dal sorgente
    // pieno).
    proxy_ranges: &[(FrameIdx, FrameIdx)],
    // Picchi audio già in memoria, a chiave `(content_hash, stream_index)`
    // del media/clip (caricati dal file di cache dal chiamante, vedi
    // `VibeVideoApp::ensure_waveforms_loaded`): disegnati come waveform
    // dentro le clip audio. `None` per un media la cui waveform non è
    // ancora pronta (il worker la sta ancora generando) — la clip si
    // disegna come prima, senza waveform. Ogni voce porta anche la
    // durata della traccia audio, base temporale dei picchi (vedi
    // `draw_clip_waveform`).
    waveform_cache: &std::collections::HashMap<(u64, usize), vv_media::Waveform>,
    // La timeline è in riproduzione: durante la riproduzione la testina deve
    // sempre restare visibile, quindi la vista "volta pagina" per
    // seguirla quando esce dall'area visibile (vedi sotto).
    playback_active: bool,
) -> Option<(TimelineDrag, FrameIdx, MediaDropTarget)> {
    let mut media_drop = None;

    // Zoom orizzontale (Alt+scroll, vedi `zoom_modifier` in `main`, o
    // pinch): solo se il puntatore è sopra il pannello, altrimenti
    // scrollare con Alt premuto altrove (es. media pool) zoomerebbe la
    // timeline per sbaglio. Le scorciatoie da tastiera/menu (zoom_in/
    // zoom_out in main.rs) mutano `pixels_per_sec` ancora prima di qui:
    // il confronto con `last_rendered_pps` sotto le cattura entrambe le
    // origini.
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
                    clip: clip.clone(),
                    label,
                    color,
                });
            }
        }
        let track_kinds: Vec<TrackKind> = tl.tracks.iter().map(|t| t.kind).collect();
        (tl.tracks.len(), track_kinds, visuals, max_end)
    };

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
    let divider_height = if video_count > 0 && audio_count > 0 {
        GROUP_DIVIDER_HEIGHT
    } else {
        0.0
    };
    let rows_height = track_count as f32 * ROW_HEIGHT + divider_height;

    // Centrate di default: lo spazio verticale non occupato dalle track si
    // divide a metà sopra/sotto, salvo che l'utente abbia trascinato il
    // separatore (`track_top_margin`).
    let avail_below_ruler = (panel_rect.height() - RULER_HEIGHT).max(0.0);
    let slack = (avail_below_ruler - rows_height).max(0.0);
    let default_top_margin = slack / 2.0;
    let top_margin = state
        .track_top_margin
        .unwrap_or(default_top_margin)
        .clamp(0.0, slack);
    // Anche zona di drop "nuova track" sopra/sotto ai gruppi (vedi sotto);
    // a zero, quella zona semplicemente sparisce.
    let bottom_margin = slack - top_margin;
    // Indipendente dall'altezza del pannello (mai in un `ui.allocate_*`,
    // altrimenti `Panel::bottom` rincorre lo spazio richiesto all'infinito).
    let content_height = RULER_HEIGHT + rows_height;
    let visual_height = RULER_HEIGHT + avail_below_ruler.max(rows_height);

    // `y` locale di ogni track (indicizzata da `track_index`), coerente con
    // `clip_local_rect`.
    let row_y: Vec<f32> = (0..track_count)
        .map(|track_index| {
            let row = row_of_track[track_index];
            let extra = if row >= video_count { divider_height } else { 0.0 };
            RULER_HEIGHT + top_margin + row as f32 * ROW_HEIGHT + extra
        })
        .collect();
    // Inverso di `row_y`: `y` locale -> riga -> `track_index`.
    let track_at_y = |local_y: f32| -> usize {
        let y_in_rows = (local_y - RULER_HEIGHT - top_margin).max(0.0);
        let video_rows_height = video_count as f32 * ROW_HEIGHT;
        let row = if y_in_rows < video_rows_height {
            (y_in_rows / ROW_HEIGHT).floor() as usize
        } else {
            let after_divider = (y_in_rows - video_rows_height - divider_height).max(0.0);
            video_count + (after_divider / ROW_HEIGHT).floor() as usize
        };
        row_order[row.min(track_count.saturating_sub(1))]
    };

    let mut pending: Option<PendingAction> = None;

    ui.horizontal_top(|ui| {
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("timeline_scroll"));
        {
            // `ctx` è un borrow immutabile di `ui`: deve finire prima del
            // `ScrollArea::show` di sotto (che prende `ui` in mutabile),
            // quindi tutto l'uso pre-`show` di `ctx` sta in questo blocco.
            let ctx = ui.ctx();
            // Attenzione: `Id::with(IdSalt)` e `Id::with(&str)` producono id
            // *diversi* per la stessa stringa — qui serve la forma IdSalt,
            // identica a quella che la ScrollArea usa in `begin` (il builder
            // `.id_salt(...)` converte in `IdSalt`).

            // Zoom ancorato alla testina: se `pixels_per_sec` è cambiato in
            // questo frame (scorciatoie da tastiera/menu — che lo mutano prima
            // del disegno — o Alt+scroll/pinch), correggiamo l'offset di
            // scroll orizzontale perché la testina resti alla stessa posizione
            // a schermo (senza di che lo zoom crescerebbe dal bordo sinistro
            // *visibile*, non da dove l'utente sta guardando). L'offset si
            // legge dallo stato persistito della ScrollArea qui sotto — stesso
            // id, dato che `make_persistent_id` usa l'id stabile dell'Ui (non
            // il contatore auto-id) — e si riscrive prima del `show`, che lo
            // rilegge in `begin`; il clamp ai limiti di contenuto resta a
            // carico di egui. (Nessun `scroll_to*` animato è usato sulla
            // timeline, quindi nessun target in corso da contrastare.)
            if state.pixels_per_sec != state.last_rendered_pps {
                if let Some(mut scroll_state) =
                    egui::containers::scroll_area::State::load(&ctx, scroll_id)
                {
                    let playhead_secs = state.playhead as f64 / fps;
                    scroll_state.offset.x +=
                        (playhead_secs as f32) * (state.pixels_per_sec - state.last_rendered_pps);
                    scroll_state.store(&ctx, scroll_id);
                }
            }
            state.last_rendered_pps = state.pixels_per_sec;

            // "Voltare pagina": durante la riproduzione la testina deve sempre
            // restare visibile — se è uscita dall'area visibile orizzontale,
            // sposta lo scroll per rimetterla in vista, posizionandola a un
            // terzo del bordo sinistro (come in un NLE). Lo scroll manuale
            // non compete: al frame successivo la vista torna a seguire la
            // testina (in riproduzione ha sempre la precedenza). Il clamp ai
            // limiti di contenuto è fatto qui con la stessa formula che egui
            // usa in `begin` (`max(0, content - available)`), così il valore
            // scritto è quello che egui manterrà.
            if playback_active {
                let viewport_width =
                    (ui.available_rect_before_wrap().width() - TRACK_HEADER_WIDTH).max(1.0);
                let playhead_x = state.playhead as f32 * px_per_frame;
                let visible_start =
                    match egui::containers::scroll_area::State::load(&ctx, scroll_id) {
                        Some(st) => st.offset.x,
                        None => 0.0,
                    };
                let visible_end = visible_start + viewport_width;
                const FOLLOW_MARGIN_FRAC: f32 = 1.0 / 3.0;
                if playhead_x < visible_start || playhead_x > visible_end {
                    let target = (playhead_x - viewport_width * FOLLOW_MARGIN_FRAC)
                        .clamp(0.0, (content_width - viewport_width).max(0.0));
                    if let Some(mut scroll_state) =
                        egui::containers::scroll_area::State::load(&ctx, scroll_id)
                    {
                        scroll_state.offset.x = target;
                        scroll_state.store(&ctx, scroll_id);
                    }
                }
            }
        }

        draw_track_headers(
            ui,
            &track_kinds,
            &row_order,
            &row_y,
            video_count,
            divider_height,
            top_margin,
            slack,
            &mut state.track_top_margin,
            content_height,
            &mut pending,
            state.playhead,
            fps,
        );

        egui::ScrollArea::horizontal()
            .id_salt("timeline_scroll")
            // `auto_shrink` di default è true su entrambi gli assi: una
            // ScrollArea si restringe al contenuto anziché riempire lo spazio
            // assegnato. Quando il contenuto (poche track corte) è più basso
            // dell'altezza a cui l'utente ha trascinato il pannello, questo fa
            // sì che il pannello *stesso* si richiuda al contenuto ogni frame
            // successivo al drag — è la causa reale del bug "il resize della
            // timeline torna indietro al rilascio del mouse": durante il drag
            // l'interazione forza la dimensione, ma al frame successivo lo
            // shrink-to-content la sovrascrive di nuovo. `false` su entrambi
            // gli assi fa riempire sempre lo spazio assegnato dal Panel.
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (rect, _resp) = ui.allocate_exact_size(
                    egui::vec2(content_width, content_height),
                    egui::Sense::hover(),
                );
                let origin = rect.min;
                // Non allocato: solo per non far ritagliare i margini di
                // centratura dal clip del painter (vedi `visual_height`).
                let visual_rect = egui::Rect::from_min_size(
                    origin,
                    egui::vec2(content_width, visual_height),
                );
                let painter = ui.painter_at(visual_rect);
                let to_local = |pos: egui::Pos2| egui::pos2(pos.x - origin.x, pos.y - origin.y);
                let press_over_a_clip = |pos: egui::Pos2| {
                    let local = to_local(pos);
                    visuals
                        .iter()
                        .any(|v| clip_local_rect(v, px_per_frame, &row_y).contains(local))
                };

                // Ruler: click/drag per spostare il playhead.
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

                // Marker temporali (tipici di un NLE: tacche maggiori con
                // etichetta mm:ss/h:mm:ss a intervallo "pulito" adattivo
                // allo zoom, più tacche minori per singolo frame quando
                // lo zoom le rende leggibili) — richiesti per correlare a
                // vista un fenomeno sulla timeline con quanti secondi/
                // frame corrisponde, invece di dover stimare a occhio
                // dalla sola larghezza delle clip.
                // Limitata al rettangolo di clip *visibile* (la ScrollArea
                // orizzontale ne ritaglia uno più stretto del content_width
                // reale): senza, una timeline lunga zoomata al livello del
                // singolo frame itererebbe migliaia di tacche fuori
                // schermo a ogni repaint.
                let visible_x = ui.clip_rect().intersect(ruler_rect);
                draw_ruler_ticks(&painter, origin, visible_x, state.pixels_per_sec, fps);

                // Striscia "buffered": una sottile fascia sul bordo inferiore
                // del righello, colorata dove ci sono già frame in cache
                // (vedi doc del parametro `buffered_ranges`). Sotto alla linea
                // della playhead (disegnata più avanti) così resta visibile
                // anche quando la playhead ci passa sopra.
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

                // Sfondo delle track (alternato per leggibilità), e sopra,
                // un'unica regione interagibile per tutta l'area sotto al
                // righello: cattura click/drag partiti da uno spazio vuoto
                // (marquee-select o "svuota selezione"). Le clip, interagite
                // più avanti nel loop, sono "sopra" a questa nell'hit-test di
                // egui (un rettangolo grande sotto + rettangoli piccoli sopra
                // è un pattern che risolve correttamente da solo), e in più il
                // controllo `press_over_a_clip` la rende no-op se il punto di
                // partenza è comunque dentro una clip: doppia sicurezza contro
                // un click che "ruba" l'interazione a una clip.
                for (row, &track_index) in row_order.iter().enumerate() {
                    let y = origin.y + row_y[track_index];
                    let track_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, y),
                        egui::vec2(content_width, ROW_HEIGHT),
                    );
                    let bg = if row % 2 == 0 {
                        egui::Color32::from_gray(32)
                    } else {
                        egui::Color32::from_gray(27)
                    };
                    painter.rect_filled(track_rect, 0.0, bg);
                }
                let track_area_rect = egui::Rect::from_min_size(
                    egui::pos2(origin.x, origin.y + RULER_HEIGHT + top_margin),
                    egui::vec2(content_width, rows_height),
                );
                // Anche i margini di centratura: il rettangolo di selezione
                // può partire da lì.
                let marquee_area_rect = egui::Rect::from_min_max(
                    egui::pos2(origin.x, origin.y + RULER_HEIGHT),
                    egui::pos2(origin.x + content_width, origin.y + visual_height),
                );
                let pointer_over_tracks = ui
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(|p| track_area_rect.contains(p));
                let marquee_resp = ui.interact(
                    marquee_area_rect,
                    ui.id().with("timeline_marquee"),
                    egui::Sense::click_and_drag(),
                );

                // Separatore trascinabile Video/Audio. Interagito *dopo*
                // `marquee_resp` per vincere l'hit-test su questa fascia
                // sottile (stesso pattern delle clip sotto).
                if divider_height > 0.0 {
                    let divider_top = origin.y
                        + RULER_HEIGHT
                        + top_margin
                        + video_count as f32 * ROW_HEIGHT;
                    let divider_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, divider_top),
                        egui::vec2(content_width, divider_height),
                    );
                    let divider_resp = ui.interact(
                        divider_rect,
                        ui.id().with("timeline_track_split"),
                        egui::Sense::drag(),
                    );
                    if divider_resp.hovered() || divider_resp.dragged() {
                        ui.ctx()
                            .output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
                    }
                    if divider_resp.dragged() {
                        state.track_top_margin =
                            Some((top_margin + divider_resp.drag_delta().y).clamp(0.0, slack));
                    }
                    let line_color = if divider_resp.hovered() || divider_resp.dragged() {
                        egui::Color32::from_gray(160)
                    } else {
                        egui::Color32::from_gray(80)
                    };
                    painter.hline(
                        divider_rect.x_range(),
                        divider_rect.center().y,
                        egui::Stroke::new(1.0, line_color),
                    );
                }

                // Drag&drop dal media pool: `dnd_hover_payload`/`dnd_release_payload`
                // guardano `contains_pointer` invece di `hovered` (che sarebbe
                // sempre false qui: il widget "attivo" durante un drag è quello
                // del media pool, non `marquee_resp`), quindi funzionano anche
                // se il drag è partito da un altro widget — vedi i loro doc in
                // egui. La posizione del rilascio va letta da `i.pointer`
                // direttamente per lo stesso motivo (`interact_pointer_pos()` è
                // legato a chi ha "vinto" l'interazione, non a questo drop).
                // Nei margini il drop spetta alle zone "nuova track" sotto.
                // Un effetto va sulla track video sotto al puntatore, un
                // media sulle track di sempre (vedi `insert_media_clip`).
                let drop_target = |drag: &TimelineDrag, pos: egui::Pos2| match drag {
                    TimelineDrag::Generator(_) => {
                        let track = track_at_y(pos.y - origin.y);
                        if track_kinds[track] == TrackKind::Video {
                            MediaDropTarget::Track(track)
                        } else {
                            MediaDropTarget::Default
                        }
                    }
                    TimelineDrag::Media(_) => MediaDropTarget::Default,
                };
                // Layer sopra alle clip, dipinte più avanti.
                let ghost_painter = painter.clone().with_layer_id(egui::LayerId::new(
                    egui::Order::Foreground,
                    ui.id().with("timeline_drop_ghost"),
                ));
                if pointer_over_tracks
                    && let Some(drag) = TimelineDrag::hovered(&marquee_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                    && !drag_set_segments(project, timeline_fps, &drag).is_empty()
                {
                    let raw_frame =
                        (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    let frame = snap_frame(
                        raw_frame,
                        drag_set_timeline_len(project, timeline_fps, &drag),
                        &visuals,
                        &[],
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0);
                    // Le track dove `insert_media_clip` mette video e audio.
                    let first_row = |kind| {
                        track_kinds.iter().position(|k| *k == kind).map(|t| origin.y + row_y[t])
                    };
                    let video_y = match drop_target(&drag, pos) {
                        MediaDropTarget::Track(track) => Some(origin.y + row_y[track]),
                        _ => first_row(TrackKind::Video),
                    };
                    let audio_y = first_row(TrackKind::Audio);
                    for seg in drag_set_segments(project, timeline_fps, &drag) {
                        let x = origin.x + (frame + seg.offset) as f32 * px_per_frame;
                        let rows = [(seg.has_video, video_y), (seg.has_audio, audio_y)];
                        for (_, y) in rows.into_iter().filter(|(present, _)| *present) {
                            let Some(y) = y else { continue };
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
                {
                    let raw_frame =
                        (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    let frame = snap_frame(
                        raw_frame,
                        drag_set_timeline_len(project, timeline_fps, &drag),
                        &visuals,
                        &[],
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0);
                    let target = drop_target(&drag, pos);
                    media_drop = Some((drag, frame, target));
                }

                // Zone "aggiungi una nuova track": margini sopra/sotto ai
                // gruppi (altezza zero se non c'è margine, vedi sopra).
                let above_video_rect = egui::Rect::from_min_size(
                    egui::pos2(origin.x, origin.y + RULER_HEIGHT),
                    egui::vec2(content_width, top_margin),
                );
                let above_video_resp = ui.interact(
                    above_video_rect,
                    ui.id().with("timeline_new_video_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&above_video_resp).is_some() {
                    painter.rect_filled(
                        above_video_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(120, 220, 120, 60),
                    );
                    painter.rect_stroke(
                        above_video_rect,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        above_video_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "+ nuova track Video",
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_rgb(200, 255, 200),
                    );
                }
                if let Some(drag) = TimelineDrag::released(&above_video_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let raw_frame =
                        (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    let frame = snap_frame(
                        raw_frame,
                        drag_set_timeline_len(project, timeline_fps, &drag),
                        &visuals,
                        &[],
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0);
                    media_drop = Some((drag, frame, MediaDropTarget::NewVideoTrack));
                }

                let below_audio_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        origin.x,
                        origin.y + RULER_HEIGHT + top_margin + rows_height,
                    ),
                    egui::vec2(content_width, bottom_margin),
                );
                let below_audio_resp = ui.interact(
                    below_audio_rect,
                    ui.id().with("timeline_new_audio_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&below_audio_resp).is_some_and(|d| d.is_media()) {
                    painter.rect_filled(
                        below_audio_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(120, 220, 120, 60),
                    );
                    painter.rect_stroke(
                        below_audio_rect,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        below_audio_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "+ nuova track Audio",
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_rgb(200, 255, 200),
                    );
                }
                if let Some(drag) = TimelineDrag::released(&below_audio_resp)
                    && drag.is_media()
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let raw_frame =
                        (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    let frame = snap_frame(
                        raw_frame,
                        drag_set_timeline_len(project, timeline_fps, &drag),
                        &visuals,
                        &[],
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0);
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
                        let hits = clips_intersecting_rect(&visuals, px_per_frame, &row_y, rect);
                        state.selected = expand_to_linked_groups(&visuals, hits.iter().copied());
                        state.selection_anchor = hits.first().copied();
                        state.selected_gap = None;
                    }
                } else if marquee_resp.clicked()
                    && let Some(pos) = marquee_resp.interact_pointer_pos()
                    && !press_over_a_clip(pos)
                {
                    if row_order.is_empty() || !track_area_rect.contains(pos) {
                        state.clear_selection();
                    } else {
                        // Click su uno spazio vuoto: se è un vuoto "vero" (seguito
                        // da un'altra clip sulla stessa track, non lo spazio in
                        // coda dopo l'ultima), lo si seleziona — comportamento alla
                        // DaVinci Resolve, dà al vuoto un'identità cliccabile e
                        // cancellabile con ripple delete (vedi `TimelineState::selected_gap`).
                        let local = to_local(pos);
                        let frame = ((local.x / px_per_frame).round() as FrameIdx).max(0);
                        let track_index = track_at_y(local.y);
                        match gap_at(&visuals, track_index, frame) {
                            Some((gap_start, gap_end)) => {
                                state.selected.clear();
                                state.selection_anchor = None;
                                state.selected_gap = Some((track_index, gap_start, gap_end));
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

                // Vuoto selezionato: stessa cornice bianca usata per una clip
                // selezionata (vedi `is_selected` più sotto), ma su un
                // rettangolo vuoto — dà al vuoto un feedback visivo di essere
                // "selezionato" come richiesto.
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

                // Track candidata per il drag in corso, dalla posizione
                // *corrente* del puntatore (vedi `track_drag_target`):
                // decide dove disegnare l'anteprima e, al rilascio, su
                // quale track atterrare.
                let drag_effective_track = state.drag.as_ref().map(|d| {
                    let kind = track_kinds[d.track_index];
                    let target = ui.input(|i| i.pointer.interact_pos()).and_then(|pos| {
                        track_drag_target(
                            to_local(pos).y,
                            kind,
                            &row_order,
                            video_count,
                            divider_height,
                            top_margin,
                            bottom_margin,
                            rows_height,
                        )
                    });
                    match target {
                        Some(TrackDragTarget::Track(idx)) => EffectiveTrack::Existing(idx),
                        Some(TrackDragTarget::NewTrack) => EffectiveTrack::New(1),
                        None => EffectiveTrack::Existing(d.track_index),
                    }
                });
                if let (Some(d), Some(EffectiveTrack::New(_))) = (&state.drag, drag_effective_track) {
                    let rect = match track_kinds[d.track_index] {
                        TrackKind::Video => above_video_rect,
                        TrackKind::Audio => below_audio_rect,
                    };
                    painter.rect_filled(
                        rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(120, 220, 120, 60),
                    );
                    painter.rect_stroke(
                        rect,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                        egui::StrokeKind::Inside,
                    );
                }

                // Vedi `drag_group_row_targets`.
                let drag_group_targets: Option<Vec<(ClipId, EffectiveTrack)>> =
                    state.drag.as_ref().map(|d| {
                        drag_group_row_targets(
                            d.clip_id,
                            d.track_index,
                            drag_effective_track.unwrap(),
                            &d.followers,
                            &track_kinds,
                            &row_of_track,
                            &row_order,
                            video_count,
                            track_count,
                        )
                    });

                // Posizione (clampata, e agganciata alla calamita se attiva)
                // della clip primaria in trascinamento, calcolata una sola
                // volta e riusata per lei e per tutto il gruppo — e per
                // l'anteprima in tempo reale durante il drag (non solo al
                // rilascio), così l'utente vede scattare le clip mentre
                // trascina. I confini si ricalcolano ogni frame sulle track
                // target correnti (vedi `drag_group_targets`), non solo
                // all'inizio del drag.
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

                // Stessa idea di `dragged_primary_new_start` ma per il trim di
                // un bordo: qui cambia anche la *lunghezza* visualizzata, non
                // solo la posizione, quindi non basta un nuovo `timeline_start`
                // da solo (vedi il calcolo di `display_start`/`display_len`
                // più sotto). Il bordo trascinato si aggancia ai bordi delle
                // altre clip come il drag di una clip intera, prima del
                // clamp di `combined_trim_range`.
                let trimmed_primary_new_value = state.trim.as_ref().map(|t| {
                    let raw = t.original_value as f32 + t.accum_px / px_per_frame;
                    let exclude: Vec<ClipId> = std::iter::once(t.clip_id)
                        .chain(t.followers.iter().map(|&(id, _, _, _)| id))
                        .collect();
                    let snapped = snap_edge(
                        raw.round() as FrameIdx,
                        &visuals,
                        &exclude,
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    );
                    snapped.clamp(t.min_value, t.max_value)
                });

                // Impostati quando il drag/trim finisce, durante il loop
                // qui sotto: `state.drag`/`state.trim` restano `Some` fino
                // a *dopo* il loop (azzerati subito sotto, non con un
                // `.take()` a metà) — altrimenti, per questo stesso frame,
                // le clip del gruppo disegnate *dopo* la primaria (le
                // successive nell'iterazione, cioè le track sottostanti)
                // leggerebbero `state.drag` già `None` e ricadrebbero un
                // istante sulla posizione pre-drag, un flash visibile
                // esattamente sulle track sotto quella trascinata (bug
                // segnalato).
                let mut drag_finished = false;
                let mut trim_finished = false;
                let mut edge_cursor: Option<(egui::Pos2, EdgeCursor)> = None;

                // Clip. Quelle in movimento (trascinate o in trim, con le
                // loro gemelle) si disegnano per ultime: sono loro a
                // invadere le altre — e a sovrascriverle al rilascio —
                // quindi devono passarci sopra, non finirci sotto.
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
                    let is_trimming_this = trimmed_keys.contains(&(visual.track_index, visual.clip.id));
                    let (display_start, display_len) = if is_trimming_this
                        && let (Some(t), Some(primary_value)) = (&state.trim, trimmed_primary_new_value)
                    {
                        let (offset, edge) = t
                            .followers
                            .iter()
                            .find(|&&(id, track, _, _)| {
                                id == visual.clip.id && track == visual.track_index
                            })
                            .map_or((0, t.edge), |&(_, _, offset, edge)| (offset, edge));
                        let new_value = primary_value + offset;
                        match edge {
                            TrimEdge::Start => {
                                (new_value, (visual.clip.timeline_end() - new_value).max(1))
                            }
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
                                .find(|(id, track, _)| {
                                    *id == visual.clip.id && *track == visual.track_index
                                }) {
                                Some((_, _, offset)) => new_start + offset,
                                None => visual.clip.timeline_start,
                            },
                            _ => visual.clip.timeline_start,
                        };
                        (start, visual.clip.timeline_len)
                    };

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
                                origin.y + RULER_HEIGHT + top_margin - *depth as f32 * ROW_HEIGHT
                            }
                            TrackKind::Audio => {
                                origin.y
                                    + RULER_HEIGHT
                                    + top_margin
                                    + rows_height
                                    + (*depth - 1) as f32 * ROW_HEIGHT
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
                    let resp = ui.interact(clip_rect, id, egui::Sense::click_and_drag());

                    // La selezione contiene sempre un gruppo collegato per
                    // intero (vedi `expand_to_linked_groups`), quindi non
                    // serve più evidenziare separatamente le gemelle non
                    // formalmente selezionate.
                    let is_selected = state.selected.contains(&(visual.track_index, visual.clip.id));
                    let stroke = if is_selected {
                        egui::Stroke::new(2.0, egui::Color32::WHITE)
                    } else {
                        egui::Stroke::new(1.0, egui::Color32::from_gray(15))
                    };
                    painter.rect_filled(clip_rect, 4.0, visual.color);
                    painter.rect_stroke(clip_rect, 4.0, stroke, egui::StrokeKind::Inside);

                    // Waveform audio: dentro la clip, solo per le clip
                    // audio di un media la cui waveform è già pronta
                    // (il worker l'ha generata e il chiamante l'ha caricata
                    // in `waveform_cache`). Il picco per ogni colonna
                    // pixel è il massimo assoluto dei bin che quella
                    // colonna copre — indipendente dallo zoom, così la
                    // forma resta la stessa a qualunque livello.
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
                            ui.clip_rect(),
                            &visual.clip.effects.gain_db,
                        );
                    }

                    // Striscia "proxy": sulla clip stessa (non sul righello
                    // della timeline, dove viveva prima) — il proxy è una
                    // proprietà *per clip*, non di un punto della timeline,
                    // quindi la sua indicazione appartiene alla clip.
                    // Etichetta spostata più in basso per non finirci sotto.
                    let is_proxy_backed = proxy_ranges.iter().any(|&(s, e)| {
                        s < visual.clip.timeline_end() && e >= visual.clip.timeline_start
                    });
                    let label_offset_y = if is_proxy_backed {
                        paint_proxy_strip(&painter, clip_rect);
                        2.0 + PROXY_STRIP_HEIGHT
                    } else {
                        2.0
                    };
                    painter.text(
                        clip_rect.left_top() + egui::vec2(4.0, label_offset_y),
                        egui::Align2::LEFT_TOP,
                        &visual.label,
                        egui::FontId::proportional(12.0),
                        egui::Color32::BLACK,
                    );
                    if visual.clip.linked_group.is_some() {
                        // Due anelli disegnati a mano invece del glifo Unicode
                        // "🔗": su alcune combinazioni piattaforma/driver (es.
                        // Asahi Linux) i font bundled di egui non lo
                        // renderizzano — appare come un quadratino vuoto.
                        let center = clip_rect.right_top() + egui::vec2(-9.0, 8.0);
                        let ring_stroke = egui::Stroke::new(1.3, egui::Color32::BLACK);
                        painter.circle_stroke(center + egui::vec2(-2.5, 0.0), 3.5, ring_stroke);
                        painter.circle_stroke(center + egui::vec2(2.5, 0.0), 3.5, ring_stroke);
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
                    if resp.hovered()
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && let Some(zone) = edge_at(pos)
                    {
                        edge_cursor = Some((pos, EdgeCursor::from_zone(zone)));
                    }

                    if resp.drag_started() {
                        // `press_origin` (il punto dove il tasto è stato
                        // premuto) invece di `interact_pointer_pos()` (dove si
                        // trova *ora*): egui richiede un piccolo movimento
                        // prima di dichiarare ufficialmente iniziato un drag su
                        // un widget che sente anche il click, quindi per il
                        // primo movimento verso l'*interno* della clip (bordo
                        // sinistro trascinato a destra, o viceversa per quello
                        // destro — il trim che "restringe") la posizione
                        // corrente a quel punto è già uscita dalla zona
                        // maniglia, mentre muoversi verso l'*esterno* (il trim
                        // che "allarga") no: da qui l'asimmetria se si usa
                        // `interact_pointer_pos()`.
                        let press_pos = ui.input(|i| i.pointer.press_origin());
                        match press_pos.and_then(edge_at) {
                            Some(zone) => {
                                let edge = zone.edge();
                                let key = (visual.track_index, visual.clip.id);
                                let others: Vec<(ClipKey, TrimEdge)> = match zone {
                                    EdgeZone::Trim(_) => {
                                        drag_group_for(&state.selected, &visuals, key)
                                            .into_iter()
                                            .filter(|k| *k != key)
                                            .map(|k| (k, edge))
                                            .collect()
                                    }
                                    // Solo le due clip a contatto, ognuna col
                                    // suo gruppo collegato.
                                    EdgeZone::Roll { neighbor, .. } => {
                                        let opposite = match edge {
                                            TrimEdge::Start => TrimEdge::End,
                                            TrimEdge::End => TrimEdge::Start,
                                        };
                                        expand_to_linked_groups(&visuals, [key])
                                            .into_iter()
                                            .filter(|k| *k != key)
                                            .map(|k| (k, edge))
                                            .chain(
                                                expand_to_linked_groups(&visuals, [neighbor])
                                                    .into_iter()
                                                    .map(|k| (k, opposite)),
                                            )
                                            .collect()
                                    }
                                };
                                let (min_value, max_value, followers) = combined_trim_range(
                                    &visuals, project, key, edge, &others,
                                );
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
                            None => {
                                let drag_group = drag_group_for(
                                    &state.selected,
                                    &visuals,
                                    (visual.track_index, visual.clip.id),
                                );
                                state.selected = drag_group.clone();
                                state.selection_anchor = Some((visual.track_index, visual.clip.id));

                                let others: Vec<ClipKey> = drag_group.into_iter().collect();
                                let (_, _, followers) = combined_drag_range(
                                    &visuals,
                                    visual.track_index,
                                    visual.clip.id,
                                    &others,
                                );

                                state.drag = Some(DragState {
                                    clip_id: visual.clip.id,
                                    track_index: visual.track_index,
                                    original_start: visual.clip.timeline_start,
                                    accum_px: 0.0,
                                    followers,
                                    duplicate: ui.input(|i| i.modifiers.alt),
                                });
                            }
                        }
                    } else if resp.dragged() {
                        if let Some(t) = &mut state.trim
                            && t.clip_id == visual.clip.id
                        {
                            t.accum_px += resp.drag_delta().x;
                        } else if let Some(d) = &mut state.drag
                            && d.clip_id == visual.clip.id
                        {
                            d.accum_px += resp.drag_delta().x;
                        }
                    } else if resp.drag_stopped() {
                        if let Some(t) = &state.trim
                            && t.clip_id == visual.clip.id
                        {
                            // Stesso valore (già clampato) mostrato
                            // nell'anteprima durante il trim: quel che si
                            // vedeva è quel che si ottiene.
                            let new_value = trimmed_primary_new_value.unwrap_or(t.original_value);
                            let mut trims = vec![(t.clip_id, t.track_index, t.edge, new_value)];
                            let mut overwritten =
                                grown_range(&visual.clip, visual.track_index, t.edge, new_value)
                                    .into_iter()
                                    .collect::<Vec<_>>();
                            for &(other_id, other_track, offset, edge) in &t.followers {
                                let Some(other) = visuals.iter().find(|v| {
                                    v.clip.id == other_id && v.track_index == other_track
                                }) else {
                                    continue;
                                };
                                let other_value = new_value + offset;
                                trims.push((other_id, other_track, edge, other_value));
                                overwritten.extend(grown_range(
                                    &other.clip,
                                    other_track,
                                    edge,
                                    other_value,
                                ));
                            }
                            pending = Some(PendingAction::Trim { trims, overwritten });
                            trim_finished = true;
                        } else if let Some(d) = &state.drag
                            && d.clip_id == visual.clip.id
                        {
                            // Stessa posizione/track (già clampata e
                            // agganciata alla calamita) mostrata
                            // nell'anteprima durante il drag: quel che si
                            // vedeva è quel che si ottiene.
                            let new_start = dragged_primary_new_start.unwrap_or(d.original_start);
                            let targets = drag_group_targets.as_deref().unwrap();
                            let original_tracks =
                                std::iter::once(d.track_index).chain(d.followers.iter().map(|(_, t, _)| *t));
                            let starts = std::iter::once(new_start)
                                .chain(d.followers.iter().map(|(_, _, offset)| new_start + offset));

                            let mut new_video_tracks = 0usize;
                            let mut new_audio_tracks = 0usize;
                            let moves: Vec<(ClipId, usize, TrackDestination, FrameIdx)> = targets
                                .iter()
                                .zip(original_tracks)
                                .zip(starts)
                                .map(|(((id, target), from_track), start)| {
                                    let dest = match *target {
                                        EffectiveTrack::Existing(track) => TrackDestination::Existing(track),
                                        EffectiveTrack::New(depth) => {
                                            let kind = track_kinds[from_track];
                                            match kind {
                                                TrackKind::Video => {
                                                    new_video_tracks = new_video_tracks.max(depth)
                                                }
                                                TrackKind::Audio => {
                                                    new_audio_tracks = new_audio_tracks.max(depth)
                                                }
                                            }
                                            TrackDestination::New(kind, depth)
                                        }
                                    };
                                    (*id, from_track, dest, start)
                                })
                                .collect();
                            pending = Some(PendingAction::Move {
                                new_video_tracks,
                                new_audio_tracks,
                                moves,
                                duplicate: d.duplicate,
                            });
                            drag_finished = true;
                        }
                    } else if resp.clicked() {
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
                    }

                    resp.context_menu(|ui| {
                        if visual.clip.linked_group.is_some() {
                            if ui.button("Scollega").clicked() {
                                pending =
                                    Some(PendingAction::Unlink(visual.track_index, visual.clip.id));
                                ui.close();
                            }
                        } else if state.selected.len() >= 2 {
                            if ui.button("Collega").clicked() {
                                pending =
                                    Some(PendingAction::Link(state.selected.iter().copied().collect()));
                                ui.close();
                            }
                        } else {
                            ui.label("Seleziona almeno 2 clip per collegarle");
                        }
                    });
                }
                // Azzerati solo ora, non con un `.take()` a metà del loop
                // sopra — vedi il commento su `drag_finished`/`trim_finished`.
                if drag_finished {
                    state.drag = None;
                }
                if trim_finished {
                    state.trim = None;
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

                // Playhead: linea verticale su tutta l'altezza, più una
                // "testina" triangolare rivolta in basso nel righello (senza,
                // la playhead era solo una linea sottile priva di un punto
                // di riferimento visivo, come in un vero NLE).
                let px = origin.x + state.playhead as f32 * px_per_frame;
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
            });

        });

    if let Some(action) = pending {
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
                    video_tracks.push(project.timelines[timeline_id].tracks.len());
                    history.do_command(
                        project,
                        Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Video)),
                    );
                }
                let mut audio_tracks = Vec::with_capacity(new_audio_tracks);
                for _ in 0..new_audio_tracks {
                    audio_tracks.push(project.timelines[timeline_id].tracks.len());
                    history.do_command(
                        project,
                        Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Audio)),
                    );
                }
                let moves: Vec<(ClipId, usize, usize, FrameIdx)> = moves
                    .into_iter()
                    .map(|(id, from_track, dest, start)| {
                        let to_track = match dest {
                            TrackDestination::Existing(track) => track,
                            TrackDestination::New(TrackKind::Video, depth) => video_tracks[depth - 1],
                            TrackDestination::New(TrackKind::Audio, depth) => audio_tracks[depth - 1],
                        };
                        (id, from_track, to_track, start)
                    })
                    .collect();
                if duplicate {
                    duplicate_clips(project, history, state, timeline_id, &moves);
                    return media_drop;
                }
                // Dove atterrano se lo prendono: quel che c'era lì viene
                // accorciato, diviso o rimosso, come per un incolla o per
                // un bordo allungato sopra la vicina. Le clip che si
                // stanno spostando restano fuori (`exclude`) — anche alla
                // loro posizione di partenza, che può cadere dentro la
                // destinazione di un'altra clip dello stesso gruppo.
                let ranges: Vec<(usize, FrameIdx, FrameIdx)> = moves
                    .iter()
                    .filter_map(|&(id, from_track, to_track, start)| {
                        let len = project.timelines[timeline_id]
                            .tracks
                            .get(from_track)?
                            .clips
                            .iter()
                            .find(|c| c.id == id)?
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
                history.do_command(project, Box::new(vv_core::CompositeCommand::new(commands)));
            }
            PendingAction::Trim { trims, overwritten } => {
                // Il tratto guadagnato allungando la clip se lo prende lei:
                // le clip che stavano lì vengono accorciate, divise o
                // rimosse prima di applicare il trim vero e proprio. Le
                // clip trimmate stesse restano fuori (`exclude`), o si
                // taglierebbero da sole.
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
                history.do_command(project, Box::new(vv_core::CompositeCommand::new(commands)));
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
            PendingAction::RemoveTrack(track_index) => {
                history.do_command(
                    project,
                    Box::new(vv_core::RemoveTrack::new(timeline_id, track_index)),
                );
                // Gli indici di track memorizzati nella selezione (e nel
                // vuoto eventualmente selezionato) possono essere shiftati
                // o non esistere più: più semplice e sicuro azzerare che
                // provare a rimapparli uno per uno.
                state.clear_selection();
            }
        }
    }

    media_drop
}

/// Inserisce una copia di ogni clip di `moves` (clip_id, from_track,
/// to_track, start) alla sua destinazione, con le stesse regole di
/// sovrascrittura di uno spostamento. Le copie di clip collegate fra loro
/// formano un gruppo nuovo, e diventano la selezione.
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
            let original = project.timelines[timeline_id]
                .tracks
                .get(from_track)?
                .clips
                .iter()
                .find(|c| c.id == id)?;
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

    let ranges: Vec<(usize, FrameIdx, FrameIdx)> = copies
        .iter()
        .map(|(track, clip, _)| (*track, clip.timeline_start, clip.timeline_end()))
        .collect();
    let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
    vv_core::make_room_for_ranges(project, timeline_id, &ranges, &[], &mut commands);

    let mut by_group: Vec<(vv_core::LinkGroupId, Vec<ClipKey>)> = Vec::new();
    let mut new_selection = BTreeSet::new();
    for (track, clip, group) in copies {
        new_selection.insert((track, clip.id));
        if let Some(group) = group {
            match by_group.iter_mut().find(|(g, _)| *g == group) {
                Some((_, members)) => members.push((track, clip.id)),
                None => by_group.push((group, vec![(track, clip.id)])),
            }
        }
        commands.push(Box::new(vv_core::InsertClip {
            timeline: timeline_id,
            track_index: track,
            clip,
        }));
    }
    for (_, targets) in by_group.into_iter().filter(|(_, t)| t.len() >= 2) {
        commands.push(Box::new(vv_core::LinkClips::new(timeline_id, targets)));
    }
    history.do_command(project, Box::new(vv_core::CompositeCommand::new(commands)));

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
                return ("⚠ Media offline".to_string(), OFFLINE_COLOR);
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
            "Solid Color".to_string(),
            darken_if_edited(egui::Color32::from_rgb(200, 170, 90), clip),
        ),
        vv_core::ClipSource::Text => (
            clip.effects
                .title
                .as_ref()
                .and_then(|t| t.content.lines().next())
                .unwrap_or("Text")
                .to_string(),
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

/// Rettangolo occupato da una clip nel disegno della timeline, in
/// coordinate locali al contenuto scrollabile (senza l'offset di
/// `origin`): condiviso dal disegno vero e proprio e dai test di
/// intersezione (marquee-select, shift+click), così i due usano
/// esattamente la stessa geometria. `row_y[visual.track_index]` dà la `y`
/// locale della riga.
fn clip_local_rect(visual: &ClipVisual, px_per_frame: f32, row_y: &[f32]) -> egui::Rect {
    let x = visual.clip.timeline_start as f32 * px_per_frame;
    let y = row_y[visual.track_index];
    let w = (visual.clip.timeline_len as f32 * px_per_frame).max(2.0);
    egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0))
}

/// Disegna la waveform di una clip audio dentro `clip_rect`: per ogni
/// colonna pixel della porzione *visibile* della clip, una linea verticale
/// centrata sull'altezza della clip, con altezza proporzionale al picco nel
/// bin corrispondente a quella colonna. `peaks` sono i picchi di *tutto* il
/// media (normalizzati in [0,1]); `clip_start_secs`/`clip_end_secs`
/// (secondi nel media) selezionano la sotto-fascia
/// temporale della clip, mappata sull'asse temporale dell'audio
/// (`audio_duration_secs`) — la stessa base temporale usata per
/// dimensionare i bin in `vv_media::waveform::generate_waveform`, così la
/// forma d'onda resta allineata al suono (e non stirata rispetto al video,
/// che può avere una durata leggermente diversa dalla traccia audio). Il bin
/// di ogni colonna è calcolato dalla sua posizione *assoluta* nel tempo
/// audio (non da un bordo di clip arrotondato e poi interpolato
/// localmente): la stessa posizione temporale mappa sempre allo stesso bin
/// a prescindere da dove cade il bordo della clip che la contiene, quindi
/// dividere una clip (`SplitClip`) non sposta la forma d'onda disegnata.
/// `visible_rect` limita il disegno alla porzione a schermo (una clip lunga
/// fuori dal viewport non viene iterata colonna per colonna).
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
        let amplified = peaks[bin] * db_to_linear(gain_db.value_at(source_frame));
        let h = (half_height * amplified.min(1.0)).max(0.5);
        painter.line_segment(
            [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
            if amplified > 1.0 { clipped_stroke } else { stroke },
        );
        x += 1.0;
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Indice del bin per una colonna a `frac` (0..1 della larghezza della
/// *clip*, non dell'intero media): calcolato dalla posizione assoluta nel
/// tempo audio (`clip_start_secs + frac*(clip_end_secs-clip_start_secs)`,
/// poi diviso per `audio_duration_secs` del media intero), non da un bordo
/// di clip arrotondato e poi interpolato localmente — vedi doc di
/// `draw_clip_waveform` sul perché: la stessa posizione temporale deve
/// mappare sempre allo stesso bin a prescindere da dove cade il bordo della
/// clip che la contiene, altrimenti dividere una clip (`SplitClip`) sposta
/// visibilmente la forma d'onda esattamente nel punto di taglio.
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
        .filter(|v| clip_local_rect(v, px_per_frame, row_y).intersects(rect))
        .map(|v| (v.track_index, v.clip.id))
        .collect()
}

/// Il vuoto sulla track `track_index` che copre `frame` (spazio frame della
/// timeline), se `frame` cade in uno spazio vuoto seguito da un'altra clip
/// sulla stessa track. Un vuoto in coda (nessuna clip dopo `frame` su quella
/// track) non conta: non c'è nulla da riavvicinare shiftandolo, quindi non
/// ha senso selezionarlo — vedi doc di `TimelineState::selected_gap`.
fn gap_at(
    visuals: &[ClipVisual],
    track_index: usize,
    frame: FrameIdx,
) -> Option<(FrameIdx, FrameIdx)> {
    let mut track_clips: Vec<&Clip> = visuals
        .iter()
        .filter(|v| v.track_index == track_index)
        .map(|v| &v.clip)
        .collect();
    track_clips.sort_by_key(|c| c.timeline_start);

    if track_clips
        .iter()
        .any(|c| frame >= c.timeline_start && frame < c.timeline_end())
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

/// I due comportamenti richiesti per il click con modificatori: `Toggle`
/// (ctrl+click) aggiunge/rimuove *solo* la clip cliccata dalla selezione
/// corrente; `Range` (shift+click) seleziona tutte le clip nel rettangolo
/// che unisce l'ancora e la clip cliccata, sostituendo la selezione
/// corrente. `Plain` (nessun modificatore) sostituisce la selezione con la
/// sola clip cliccata.
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

/// Come `drag_range`, ma per una clip lunga `len` valutata a
/// `reference_start` su `track_index` senza doverci già essere, e
/// ignorando `exclude` invece del solo `clip_id` (usato per un intero
/// gruppo in trascinamento, vedi `group_drag_bounds`).
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

/// Espande un insieme di clip alla chiusura dei loro gruppi collegati
/// (`Clip::linked_group`): se una clip è collegata, l'intero gruppo entra
/// nel risultato, non solo lei. Selezione, drag e cancellazione trattano un
/// gruppo come un'unità (vedi doc del modulo) — questa è l'unica funzione
/// che materializza quell'invariante, chiamata da ogni punto che scrive
/// `TimelineState::selected` a partire da un'interazione diretta con la
/// timeline (click, ctrl+click, shift+click, rettangolo).
fn expand_to_linked_groups(
    visuals: &[ClipVisual],
    keys: impl IntoIterator<Item = ClipKey>,
) -> BTreeSet<ClipKey> {
    let mut result: BTreeSet<ClipKey> = BTreeSet::new();
    for key @ (track_index, clip_id) in keys {
        result.insert(key);
        let group = visuals
            .iter()
            .find(|v| v.track_index == track_index && v.clip.id == clip_id)
            .and_then(|v| v.clip.linked_group);
        if let Some(group) = group {
            for v in visuals.iter().filter(|v| v.clip.linked_group == Some(group)) {
                result.insert((v.track_index, v.clip.id));
            }
        }
    }
    result
}

/// L'insieme di clip che deve muoversi insieme quando si inizia un drag su
/// `clicked` (bug segnalato: trascinare una clip dentro una multi-selezione
/// non spostava le altre). Se `clicked` fa già parte di `selected`, il drag
/// segue *l'intera selezione corrente* — che contiene sempre un gruppo
/// collegato per intero, vedi `expand_to_linked_groups`, quindi non serve
/// unirla esplicitamente al gruppo qui. Se `clicked` non è selezionata,
/// trascinarla sostituisce la selezione con lei (+ il suo gruppo
/// collegato) — comportamento standard da NLE: un drag su una clip fuori
/// dalla selezione corrente non deve trascinarsi dietro una selezione
/// precedente e scorrelata.
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

/// Range valido per il `timeline_start` di `clip_id`, combinato con quello
/// di ogni altra clip in `others` (tipicamente l'intera selezione corrente,
/// vedi il chiamante): il drag deve rispettare i vincoli di *tutte*,
/// tradotti nello spazio della clip primaria. Restituisce anche (id, track,
/// offset) di ognuna, pronti per essere salvati in `DragState::followers`.
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
        min_start = min_start.max(o_min - offset);
        max_start = max_start.min(o_max - offset);
        followers.push((other_id, other_track, offset));
    }
    (min_start, max_start, followers)
}

/// Come `combined_drag_range`, ma ogni clip del gruppo (primaria in
/// `targets[0]`, poi un elemento di `targets` per ogni `followers`, stesso
/// ordine) può atterrare su una track diversa dalla propria — vedi
/// `EffectiveTrack` e il calcolo dei target in `show_timeline` (spostamento
/// di gruppo fra track, non solo orizzontale). I vicini considerati per
/// ciascuna escludono sempre l'intero gruppo, non solo la clip valutata,
/// altrimenti due clip del gruppo destinate alla stessa track si
/// bloccherebbero a vicenda.
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
        min_start = min_start.max(o_min - offset);
        max_start = max_start.min(o_max - offset);
    }
    (min_start, max_start.max(min_start))
}

/// Range valido (in frame timeline) per il nuovo valore del bordo `edge`
/// della primaria, combinato con quello di ogni clip in `others` (col
/// proprio bordo) tradotto nello spazio della primaria — stesso principio
/// di `combined_drag_range`. Ritorna anche (clip_id, track_index, offset,
/// bordo) di ognuna, pronti per `TrimState::followers`.
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

/// I vicini sulla track non limitano il trim: allungando un bordo oltre
/// un'altra clip la si sovrascrive (`make_room_for_ranges` al rilascio),
/// come in un vero NLE. I soli limiti sono il sorgente e il bordo opposto
/// della clip stessa.
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

/// Come `snap_frame`, ma per il singolo bordo trascinato in un trim: non
/// c'è una clip da allineare per intero, solo il punto che si sta
/// spostando, che si aggancia al bordo di clip più vicino entro soglia.
fn snap_edge(
    candidate: FrameIdx,
    visuals: &[ClipVisual],
    exclude: &[ClipId],
    extra_targets: &[FrameIdx],
    px_per_frame: f32,
    enabled: bool,
) -> FrameIdx {
    if !enabled {
        return candidate;
    }
    let threshold = (SNAP_THRESHOLD_PX / px_per_frame).round() as FrameIdx;
    if threshold <= 0 {
        return candidate;
    }
    snap_targets(visuals, exclude, extra_targets)
        .map(|edge| ((candidate - edge).abs(), edge))
        .filter(|&(delta, _)| delta <= threshold)
        .min_by_key(|&(delta, _)| delta)
        .map_or(candidate, |(_, edge)| edge)
}

/// Se la calamita è attiva, aggancia `candidate_start` (una clip lunga
/// `len` frame) al bordo più vicino — inizio o fine — di un'altra clip
/// della timeline, se entro `SNAP_THRESHOLD_PX` pixel: allinea o l'inizio
/// o la fine della clip trascinata, qualunque dei due richieda lo scarto
/// minore. Le clip in `exclude` (la clip trascinata stessa e l'eventuale
/// gemella collegata, o nessuna per una clip nuova dal media pool) non
/// sono bordi validi. No-op se `enabled` è `false` o se nulla è entro
/// soglia.
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

const PROXY_STRIP_HEIGHT: f32 = 4.0;
/// Colore dell'indicatore "proxy disponibile", condiviso col media pool.
/// Clip il cui media non è più nel media pool.
pub const OFFLINE_COLOR: egui::Color32 = egui::Color32::from_rgb(170, 50, 50);

pub const PROXY_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(220, 151, 52, 220);

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
    }

    /// `row_y` "identità" (nessun raggruppamento/margine) per i test.
    fn test_row_y(n: usize) -> Vec<f32> {
        (0..n).map(|i| RULER_HEIGHT + i as f32 * ROW_HEIGHT).collect()
    }

    fn visual(track_index: usize, id: u64, start: FrameIdx, len: FrameIdx) -> ClipVisual {
        ClipVisual {
            track_index,
            clip: Clip::from_source_range(
                ClipId(id),
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            ),
            label: String::new(),
            color: egui::Color32::WHITE,
        }
    }

    fn visual_linked(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        len: FrameIdx,
        group: u64,
    ) -> ClipVisual {
        let mut v = visual(track_index, id, start, len);
        v.clip.linked_group = Some(vv_core::LinkGroupId(group));
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
    fn track_drag_target_above_video_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2]; // 2 track video (decrescente), 1 audio
        let target = track_drag_target(40.0, TrackKind::Video, &row_order, 2, 8.0, 50.0, 50.0, 128.0);
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_above_video_group_is_none_without_margin() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(15.0, TrackKind::Video, &row_order, 2, 8.0, 0.0, 0.0, 128.0);
        assert!(target.is_none());
    }

    #[test]
    fn track_drag_target_lands_on_the_right_video_row() {
        let row_order = [1, 0, 2];
        let first_row = track_drag_target(80.0, TrackKind::Video, &row_order, 2, 8.0, 50.0, 50.0, 128.0);
        assert!(matches!(first_row, Some(TrackDragTarget::Track(1))));
        let second_row = track_drag_target(120.0, TrackKind::Video, &row_order, 2, 8.0, 50.0, 50.0, 128.0);
        assert!(matches!(second_row, Some(TrackDragTarget::Track(0))));
    }

    #[test]
    fn track_drag_target_video_over_audio_group_is_none() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(170.0, TrackKind::Video, &row_order, 2, 8.0, 50.0, 50.0, 128.0);
        assert!(target.is_none());
    }

    #[test]
    fn track_drag_target_below_audio_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, 2, 8.0, 50.0, 50.0, 128.0);
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_below_audio_group_is_none_without_margin() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, 2, 8.0, 50.0, 0.0, 128.0);
        assert!(target.is_none());
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
    ) -> ClipVisual {
        ClipVisual {
            track_index,
            clip: Clip::from_source_range(
                ClipId(id),
                ClipSource::Media(media_id),
                source_in,
                source_out,
                start,
                vv_core::Rational::one(),
            ),
            label: String::new(),
            color: egui::Color32::WHITE,
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
            },
            content_hash: 0,
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
            },
            content_hash: 1,
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
        visuals[0].clip = Clip::from_source_range(
            visuals[0].clip.id,
            visuals[0].clip.source.clone(),
            2000,
            2400,
            3000,
            vv_core::Rational::conform_rate(
                vv_core::Rational::new(30, 1),
                vv_core::Rational::new(30_000, 1001),
            ),
        );

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
        video.clip.linked_group = group;
        let mut audio = media_clip_visual(1, 2, 10, 0, 20, media_id);
        audio.clip.linked_group = group;
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
        second.clip = Clip::from_source_range(
            ClipId(2),
            vv_core::ClipSource::SolidColor,
            5,
            20,
            10,
            vv_core::Rational::one(),
        );
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
    fn snap_edge_snaps_to_the_playhead() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_edge(14, &visuals, &[ClipId(1)], &[15], 5.0, true), 15);
        assert_eq!(snap_frame(4, 10, &visuals, &[ClipId(1)], &[15], 5.0, true), 5);
    }

    #[test]
    fn snap_edge_snaps_the_trimmed_edge_to_a_nearby_clip_edge() {
        // Clip vicina [20,30): il bordo trascinato a 18, entro soglia
        // (10px / 5px per frame = 2 frame), si aggancia a 20.
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_edge(18, &visuals, &[ClipId(1)], &[], 5.0, true), 20);
    }

    #[test]
    fn snap_edge_ignores_the_clip_being_trimmed_and_far_edges() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        // Il proprio bordo (10) non è un aggancio valido.
        assert_eq!(snap_edge(11, &visuals, &[ClipId(1)], &[], 5.0, true), 11);
        // Fuori soglia: nessun aggancio.
        assert_eq!(snap_edge(15, &visuals, &[ClipId(1)], &[], 5.0, true), 15);
        // Calamita spenta: nessun aggancio nemmeno entro soglia.
        assert_eq!(snap_edge(18, &visuals, &[ClipId(1)], &[], 5.0, false), 18);
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
            ctx.run_ui(frame_input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        captured = Some(ui.make_persistent_id(egui::IdSalt::new("timeline_scroll")));
                    });
                });
            });
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
