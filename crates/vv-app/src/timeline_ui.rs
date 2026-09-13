//! Widget timeline multi-traccia: disegna tracce/clip, gestisce
//! multi-selezione (click semplice, ctrl+click per aggiungere/togglere,
//! shift+click per un range, rettangolo di selezione trascinando da
//! un'area vuota), drag orizzontale (con clip collegate che si muovono
//! insieme, vedi `Clip::linked`), menu contestuale per collegare/scollegare,
//! e il playhead. Ogni mutazione del progetto passa da
//! `History::do_command`, mai da una modifica diretta del `Project`; i
//! cambi di sola selezione invece mutano `TimelineState` direttamente,
//! visto che non toccano `project`/`history`.
//!
//! Disegno "immediate mode" a basso livello (painter diretto, non widget
//! egui nidificati): per una griglia densa di rettangoli come una timeline
//! dà più controllo e meno overhead dei container annidati.

use std::collections::{BTreeSet, HashSet};

use vv_core::{Clip, ClipId, FrameIdx, History, Project, TimelineId, Track, TrackKind};

const ROW_HEIGHT: f32 = 40.0;
const RULER_HEIGHT: f32 = 20.0;
const MIN_TIMELINE_SECS: f64 = 20.0;
const TRAILING_MARGIN_SECS: f64 = 5.0;

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
}

struct MarqueeDrag {
    start: egui::Pos2,
    current: egui::Pos2,
}

struct DragState {
    clip_id: ClipId,
    track_index: usize,
    original_start: FrameIdx,
    accum_px: f32,
    /// Range valido per il *nuovo `timeline_start` della clip primaria*,
    /// già combinato con quello della gemella collegata se presente (vedi
    /// `drag_range`): min/max, non un "upper" grezzo da cui sottrarre la
    /// lunghezza a ogni uso.
    min_start: FrameIdx,
    max_start: FrameIdx,
    /// (clip_id, track_index, offset) della gemella collegata, se c'è:
    /// `offset` è la distanza fissa `gemella.timeline_start -
    /// primaria.timeline_start` catturata all'inizio del drag.
    linked: Option<(ClipId, usize, FrameIdx)>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            selection_anchor: None,
            playhead: 0,
            pixels_per_sec: 60.0,
            drag: None,
            marquee: None,
            selected_gap: None,
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
}

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
    /// (clip_id, track_index, new_start) per una o due clip (collegate).
    Move(Vec<(ClipId, usize, FrameIdx)>),
    Unlink(usize, ClipId),
    Link(usize, ClipId, usize, ClipId),
}

/// Ritorna `Some((media, frame))` se in questo frame l'utente ha rilasciato
/// sulla timeline un elemento trascinato dal media pool: il chiamante (che
/// ha accesso al media pool e alla history) se ne occupa, questa funzione si
/// limita a disegnare l'anteprima del punto di atterraggio e a calcolare il
/// frame dalla posizione orizzontale del rilascio.
pub fn show_timeline(
    ui: &mut egui::Ui,
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
    state: &mut TimelineState,
    snapping_enabled: bool,
) -> Option<(vv_core::MediaId, FrameIdx)> {
    let mut media_drop = None;
    let fps = project.timelines[timeline_id].fps.as_f64();
    let px_per_frame = state.pixels_per_sec / fps.max(1.0) as f32;

    // --- pass 1: raccogli i dati da disegnare (borrow immutabile) ---
    let (track_count, visuals, max_end_frames) = {
        let tl = &project.timelines[timeline_id];
        let mut visuals = Vec::new();
        let mut max_end: FrameIdx = 0;
        for (track_index, track) in tl.tracks.iter().enumerate() {
            for clip in &track.clips {
                max_end = max_end.max(clip.timeline_end());
                let (label, color) = clip_label_and_color(clip, track, media_labels);
                visuals.push(ClipVisual {
                    track_index,
                    clip: clip.clone(),
                    label,
                    color,
                });
            }
        }
        (tl.tracks.len(), visuals, max_end)
    };

    let total_secs = (max_end_frames as f64 / fps + TRAILING_MARGIN_SECS).max(MIN_TIMELINE_SECS);
    let content_width = (total_secs * state.pixels_per_sec as f64) as f32;
    let content_height = RULER_HEIGHT + track_count as f32 * ROW_HEIGHT;

    let mut pending: Option<PendingAction> = None;

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
            let painter = ui.painter_at(rect);
            let origin = rect.min;
            let to_local = |pos: egui::Pos2| egui::pos2(pos.x - origin.x, pos.y - origin.y);
            let press_over_a_clip = |pos: egui::Pos2| {
                let local = to_local(pos);
                visuals
                    .iter()
                    .any(|v| clip_local_rect(v, px_per_frame).contains(local))
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
            if let Some(pos) = ruler_resp.interact_pointer_pos() {
                let frame = (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                state.playhead = frame;
            }
            if ruler_resp.clicked() {
                state.clear_selection();
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
            for track_index in 0..track_count {
                let y = origin.y + RULER_HEIGHT + track_index as f32 * ROW_HEIGHT;
                let track_rect = egui::Rect::from_min_size(
                    egui::pos2(origin.x, y),
                    egui::vec2(content_width, ROW_HEIGHT),
                );
                let bg = if track_index % 2 == 0 {
                    egui::Color32::from_gray(32)
                } else {
                    egui::Color32::from_gray(27)
                };
                painter.rect_filled(track_rect, 0.0, bg);
            }
            let track_area_rect = egui::Rect::from_min_size(
                egui::pos2(origin.x, origin.y + RULER_HEIGHT),
                egui::vec2(content_width, content_height - RULER_HEIGHT),
            );
            let marquee_resp = ui.interact(
                track_area_rect,
                ui.id().with("timeline_marquee"),
                egui::Sense::click_and_drag(),
            );

            // Drag&drop dal media pool: `dnd_hover_payload`/`dnd_release_payload`
            // guardano `contains_pointer` invece di `hovered` (che sarebbe
            // sempre false qui: il widget "attivo" durante un drag è quello
            // del media pool, non `marquee_resp`), quindi funzionano anche
            // se il drag è partito da un altro widget — vedi i loro doc in
            // egui. La posizione del rilascio va letta da `i.pointer`
            // direttamente per lo stesso motivo (`interact_pointer_pos()` è
            // legato a chi ha "vinto" l'interazione, non a questo drop).
            if let Some(media_id) = marquee_resp.dnd_hover_payload::<vv_core::MediaId>()
                && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                && let Some(item) = project.media_pool.get(*media_id)
            {
                let raw_frame = (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                let frame = snap_frame(
                    raw_frame,
                    item.meta.duration_frames,
                    &visuals,
                    &[],
                    px_per_frame,
                    snapping_enabled,
                )
                .max(0);
                let ghost_height = if item.meta.has_audio {
                    2.0 * ROW_HEIGHT
                } else {
                    ROW_HEIGHT
                };
                let ghost_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        origin.x + frame as f32 * px_per_frame,
                        origin.y + RULER_HEIGHT,
                    ),
                    egui::vec2(
                        item.meta.duration_frames as f32 * px_per_frame,
                        ghost_height,
                    ),
                );
                painter.rect_filled(
                    ghost_rect,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(120, 220, 120, 90),
                );
                painter.rect_stroke(
                    ghost_rect,
                    4.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(media_id) = marquee_resp.dnd_release_payload::<vv_core::MediaId>()
                && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
            {
                let raw_frame = (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                let len = project
                    .media_pool
                    .get(*media_id)
                    .map(|item| item.meta.duration_frames)
                    .unwrap_or(0);
                let frame = snap_frame(
                    raw_frame,
                    len,
                    &visuals,
                    &[],
                    px_per_frame,
                    snapping_enabled,
                )
                .max(0);
                media_drop = Some((*media_id, frame));
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
                    let hits = clips_intersecting_rect(&visuals, px_per_frame, rect);
                    state.selected = hits.iter().copied().collect();
                    state.selection_anchor = hits.first().copied();
                    state.selected_gap = None;
                }
            } else if marquee_resp.clicked()
                && let Some(pos) = marquee_resp.interact_pointer_pos()
                && !press_over_a_clip(pos)
            {
                // Click su uno spazio vuoto: se è un vuoto "vero" (seguito
                // da un'altra clip sulla stessa track, non lo spazio in
                // coda dopo l'ultima), lo si seleziona — comportamento alla
                // DaVinci Resolve, dà al vuoto un'identità cliccabile e
                // cancellabile con ripple delete (vedi `TimelineState::selected_gap`).
                let local = to_local(pos);
                let frame = ((local.x / px_per_frame).round() as FrameIdx).max(0);
                let track_index = ((local.y - RULER_HEIGHT) / ROW_HEIGHT).floor().max(0.0) as usize;
                let track_index = track_index.min(track_count.saturating_sub(1));
                match gap_at(&visuals, track_index, frame) {
                    Some((gap_start, gap_end)) => {
                        state.selected.clear();
                        state.selection_anchor = None;
                        state.selected_gap = Some((track_index, gap_start, gap_end));
                    }
                    None => state.clear_selection(),
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
            if let Some((track_index, gap_start, gap_end)) = state.selected_gap {
                let y = origin.y + RULER_HEIGHT + track_index as f32 * ROW_HEIGHT;
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

            // Posizione (clampata, e agganciata alla calamita se attiva)
            // della clip primaria in trascinamento, calcolata una sola
            // volta e riusata sia per lei sia per l'eventuale gemella
            // collegata — e per l'anteprima in tempo reale durante il drag
            // (non solo al rilascio), così l'utente vede scattare la clip
            // mentre trascina.
            let dragged_primary_new_start = state.drag.as_ref().map(|d| {
                let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                let candidate = (raw.round() as FrameIdx).clamp(d.min_start, d.max_start);
                let len = visuals
                    .iter()
                    .find(|v| v.clip.id == d.clip_id)
                    .map(|v| v.clip.timeline_len())
                    .unwrap_or(0);
                let mut exclude = vec![d.clip_id];
                if let Some((partner_id, _, _)) = d.linked {
                    exclude.push(partner_id);
                }
                snap_frame(
                    candidate,
                    len,
                    &visuals,
                    &exclude,
                    px_per_frame,
                    snapping_enabled,
                )
                .clamp(d.min_start, d.max_start)
            });

            // Le gemelle collegate di tutte le clip selezionate vanno
            // evidenziate insieme a loro (le clip audio+video sono
            // collegate di default): calcolato una volta sola, non per
            // ogni clip.
            let linked_ids = selected_linked_clip_ids(&visuals, &state.selected);

            // Clip.
            for visual in &visuals {
                let display_start = match (&state.drag, dragged_primary_new_start) {
                    (Some(d), Some(new_start)) if d.clip_id == visual.clip.id => new_start,
                    (Some(d), Some(new_start)) => match d.linked {
                        Some((partner_id, partner_track, offset))
                            if partner_id == visual.clip.id
                                && partner_track == visual.track_index =>
                        {
                            new_start + offset
                        }
                        _ => visual.clip.timeline_start,
                    },
                    _ => visual.clip.timeline_start,
                };

                let x = origin.x + display_start as f32 * px_per_frame;
                let y = origin.y + RULER_HEIGHT + visual.track_index as f32 * ROW_HEIGHT;
                let w = (visual.clip.timeline_len() as f32 * px_per_frame).max(2.0);
                let clip_rect = egui::Rect::from_min_size(
                    egui::pos2(x, y + 2.0),
                    egui::vec2(w, ROW_HEIGHT - 4.0),
                );

                let id = ui.id().with("clip").with(visual.clip.id.0);
                let resp = ui.interact(clip_rect, id, egui::Sense::click_and_drag());

                let is_selected = state
                    .selected
                    .contains(&(visual.track_index, visual.clip.id))
                    || linked_ids.contains(&visual.clip.id);
                let stroke = if is_selected {
                    egui::Stroke::new(2.0, egui::Color32::WHITE)
                } else {
                    egui::Stroke::new(1.0, egui::Color32::from_gray(15))
                };
                painter.rect_filled(clip_rect, 4.0, visual.color);
                painter.rect_stroke(clip_rect, 4.0, stroke, egui::StrokeKind::Inside);
                painter.text(
                    clip_rect.left_top() + egui::vec2(4.0, 2.0),
                    egui::Align2::LEFT_TOP,
                    &visual.label,
                    egui::FontId::proportional(12.0),
                    egui::Color32::BLACK,
                );
                if visual.clip.linked.is_some() {
                    // Due anelli disegnati a mano invece del glifo Unicode
                    // "🔗": su alcune combinazioni piattaforma/driver (es.
                    // Asahi Linux) i font bundled di egui non lo
                    // renderizzano — appare come un quadratino vuoto.
                    let center = clip_rect.right_top() + egui::vec2(-9.0, 8.0);
                    let ring_stroke = egui::Stroke::new(1.3, egui::Color32::BLACK);
                    painter.circle_stroke(center + egui::vec2(-2.5, 0.0), 3.5, ring_stroke);
                    painter.circle_stroke(center + egui::vec2(2.5, 0.0), 3.5, ring_stroke);
                }

                if resp.drag_started() {
                    let (min_start, max_start, linked) = combined_drag_range(
                        &visuals,
                        visual.track_index,
                        visual.clip.id,
                        visual.clip.linked,
                    );

                    state.drag = Some(DragState {
                        clip_id: visual.clip.id,
                        track_index: visual.track_index,
                        original_start: visual.clip.timeline_start,
                        accum_px: 0.0,
                        min_start,
                        max_start: max_start.max(min_start),
                        linked,
                    });
                } else if resp.dragged() {
                    if let Some(d) = &mut state.drag
                        && d.clip_id == visual.clip.id
                    {
                        d.accum_px += resp.drag_delta().x;
                    }
                } else if resp.drag_stopped() {
                    if let Some(d) = state.drag.take()
                        && d.clip_id == visual.clip.id
                    {
                        // Stessa posizione (già clampata e agganciata alla
                        // calamita) mostrata nell'anteprima durante il drag,
                        // calcolata da `state.drag` prima del `take()` qui
                        // sopra: quel che si vedeva è quel che si ottiene.
                        let new_start = dragged_primary_new_start.unwrap_or(d.original_start);
                        let mut moves = vec![(d.clip_id, d.track_index, new_start)];
                        if let Some((partner_id, partner_track, offset)) = d.linked {
                            moves.push((partner_id, partner_track, new_start + offset));
                        }
                        pending = Some(PendingAction::Move(moves));
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
                    );
                    state.selected = selected;
                    state.selection_anchor = anchor;
                    state.selected_gap = None;
                }

                resp.context_menu(|ui| {
                    if visual.clip.linked.is_some() {
                        if ui.button("Scollega audio/video").clicked() {
                            pending =
                                Some(PendingAction::Unlink(visual.track_index, visual.clip.id));
                            ui.close();
                        }
                    } else if state.selected.len() == 2 {
                        if ui.button("Collega").clicked() {
                            let mut two = state.selected.iter().copied();
                            let a = two.next().expect("len() == 2");
                            let b = two.next().expect("len() == 2");
                            pending = Some(PendingAction::Link(a.0, a.1, b.0, b.1));
                            ui.close();
                        }
                    } else {
                        ui.label("Seleziona esattamente 2 clip per collegarle");
                    }
                });
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
                    egui::pos2(px, origin.y + content_height),
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

    if let Some(action) = pending {
        match action {
            PendingAction::Move(moves) => {
                let moves = moves
                    .into_iter()
                    .map(|(id, track, start)| (id, track, track, start))
                    .collect();
                history.do_command(
                    project,
                    Box::new(vv_core::MoveClips::new(timeline_id, moves)),
                );
            }
            PendingAction::Unlink(track_index, clip_id) => {
                history.do_command(
                    project,
                    Box::new(vv_core::UnlinkClip::new(timeline_id, track_index, clip_id)),
                );
            }
            PendingAction::Link(track_a, clip_a, track_b, clip_b) => {
                history.do_command(
                    project,
                    Box::new(vv_core::LinkClips::new(
                        timeline_id,
                        (track_a, clip_a),
                        (track_b, clip_b),
                    )),
                );
            }
        }
    }

    media_drop
}

fn clip_label_and_color(
    clip: &Clip,
    track: &Track,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
) -> (String, egui::Color32) {
    match &clip.source {
        vv_core::ClipSource::Media(media_id) => {
            let label = media_labels(*media_id);
            let color = if track.kind == TrackKind::Video {
                egui::Color32::from_rgb(90, 140, 200)
            } else {
                egui::Color32::from_rgb(90, 190, 140)
            };
            (label, color)
        }
        vv_core::ClipSource::SolidColor => (
            "Solid Color".to_string(),
            egui::Color32::from_rgb(200, 170, 90),
        ),
    }
}

/// Rettangolo occupato da una clip nel disegno della timeline, in
/// coordinate locali al contenuto scrollabile (senza l'offset di
/// `origin`): condiviso dal disegno vero e proprio e dai test di
/// intersezione (marquee-select, shift+click), così i due usano
/// esattamente la stessa geometria.
fn clip_local_rect(visual: &ClipVisual, px_per_frame: f32) -> egui::Rect {
    let x = visual.clip.timeline_start as f32 * px_per_frame;
    let y = RULER_HEIGHT + visual.track_index as f32 * ROW_HEIGHT;
    let w = (visual.clip.timeline_len() as f32 * px_per_frame).max(2.0);
    egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0))
}

/// Le clip il cui rettangolo interseca `rect` (coordinate locali): nucleo
/// condiviso da marquee-select e shift+click (che usa il rettangolo che
/// unisce l'ancora e la clip cliccata).
fn clips_intersecting_rect(
    visuals: &[ClipVisual],
    px_per_frame: f32,
    rect: egui::Rect,
) -> Vec<ClipKey> {
    visuals
        .iter()
        .filter(|v| clip_local_rect(v, px_per_frame).intersects(rect))
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
                .map(|v| clip_local_rect(v, px_per_frame));
            let clicked_rect = visuals
                .iter()
                .find(|v| (v.track_index, v.clip.id) == clicked)
                .map(|v| clip_local_rect(v, px_per_frame));
            let set = match (anchor_rect, clicked_rect) {
                (Some(a), Some(c)) => clips_intersecting_rect(visuals, px_per_frame, a.union(c))
                    .into_iter()
                    .collect(),
                _ => BTreeSet::from([clicked]),
            };
            (set, Some(effective_anchor))
        }
    }
}

/// Confini imposti dai vicini immediati (precedente/successivo) della clip
/// sulla stessa track, calcolati una sola volta all'inizio del drag: la
/// clip non può attraversarli (comportamento standard "sposta senza
/// ripple": per superare un vicino serve prima un'altra operazione).
fn neighbor_bounds(
    visuals: &[ClipVisual],
    track_index: usize,
    moving_id: ClipId,
) -> (FrameIdx, FrameIdx) {
    let Some(moving) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == moving_id)
    else {
        return (0, FrameIdx::MAX);
    };

    let mut lower_bound: FrameIdx = 0;
    let mut upper_bound: FrameIdx = FrameIdx::MAX;

    for v in visuals {
        if v.track_index != track_index || v.clip.id == moving_id {
            continue;
        }
        if v.clip.timeline_end() <= moving.clip.timeline_start {
            lower_bound = lower_bound.max(v.clip.timeline_end());
        }
        if v.clip.timeline_start >= moving.clip.timeline_end() {
            upper_bound = upper_bound.min(v.clip.timeline_start);
        }
    }

    (lower_bound, upper_bound)
}

fn max_start_in_slot(lower: FrameIdx, upper: FrameIdx, len: FrameIdx) -> FrameIdx {
    upper.saturating_sub(len).max(lower)
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
    let (lower, upper) = neighbor_bounds(visuals, track_index, clip_id);
    (
        lower,
        max_start_in_slot(lower, upper, v.clip.timeline_len()),
    )
}

/// Gli id delle gemelle collegate di tutte le clip selezionate: vanno
/// evidenziate insieme alla selezione (le clip audio+video sono collegate
/// di default) anche se non fanno formalmente parte di `selected`.
fn selected_linked_clip_ids(
    visuals: &[ClipVisual],
    selected: &BTreeSet<ClipKey>,
) -> HashSet<ClipId> {
    visuals
        .iter()
        .filter(|v| selected.contains(&(v.track_index, v.clip.id)))
        .filter_map(|v| v.clip.linked)
        .collect()
}

/// Range valido per il `timeline_start` di `clip_id`, combinato con quello
/// della sua gemella collegata (se `linked` è `Some`): il drag deve
/// rispettare i vincoli di *entrambe*, tradotti nello spazio della clip
/// primaria. Restituisce anche (id, track, offset) della gemella, pronti
/// per essere salvati in `DragState`.
fn combined_drag_range(
    visuals: &[ClipVisual],
    track_index: usize,
    clip_id: ClipId,
    linked: Option<ClipId>,
) -> (FrameIdx, FrameIdx, Option<(ClipId, usize, FrameIdx)>) {
    let (min_start, max_start) = drag_range(visuals, track_index, clip_id);

    let Some(partner_id) = linked else {
        return (min_start, max_start, None);
    };
    let Some(partner) = visuals.iter().find(|v| v.clip.id == partner_id) else {
        return (min_start, max_start, None);
    };
    let Some(this_start) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
        .map(|v| v.clip.timeline_start)
    else {
        return (min_start, max_start, None);
    };

    let offset = partner.clip.timeline_start - this_start;
    let (p_min, p_max) = drag_range(visuals, partner.track_index, partner_id);
    (
        min_start.max(p_min - offset),
        max_start.min(p_max - offset),
        Some((partner_id, partner.track_index, offset)),
    )
}

/// Soglia di aggancio della calamita, in pixel schermo (non in frame:
/// resta la stessa distanza visiva a qualunque livello di zoom, convertita
/// in frame da `snap_frame` in base a `px_per_frame`).
const SNAP_THRESHOLD_PX: f32 = 10.0;

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
    for v in visuals {
        if exclude.contains(&v.clip.id) {
            continue;
        }
        for edge in [v.clip.timeline_start, v.clip.timeline_end()] {
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
    }
    best.map_or(candidate_start, |(_, new_start)| new_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual(track_index: usize, id: u64, start: FrameIdx, len: FrameIdx) -> ClipVisual {
        ClipVisual {
            track_index,
            clip: Clip {
                id: ClipId(id),
                source: vv_core::ClipSource::SolidColor,
                source_in: 0,
                source_out: len,
                timeline_start: start,
                effects: vv_core::EffectStack::default(),
                linked: None,
            },
            label: String::new(),
            color: egui::Color32::WHITE,
        }
    }

    fn visual_linked(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        len: FrameIdx,
        linked: u64,
    ) -> ClipVisual {
        let mut v = visual(track_index, id, start, len);
        v.clip.linked = Some(ClipId(linked));
        v
    }

    #[test]
    fn selected_linked_clip_ids_finds_the_partners() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 2), visual_linked(1, 2, 0, 10, 1)];
        let selected = BTreeSet::from([(0, ClipId(1))]);
        assert_eq!(
            selected_linked_clip_ids(&visuals, &selected),
            HashSet::from([ClipId(2)]),
            "selezionando il video deve trovare l'audio collegato"
        );
        let selected = BTreeSet::from([(1, ClipId(2))]);
        assert_eq!(
            selected_linked_clip_ids(&visuals, &selected),
            HashSet::from([ClipId(1)]),
            "e viceversa, selezionando l'audio deve trovare il video"
        );
    }

    #[test]
    fn selected_linked_clip_ids_is_empty_when_unlinked_or_unselected() {
        let visuals = vec![visual(0, 1, 0, 10)];
        assert!(selected_linked_clip_ids(&visuals, &BTreeSet::from([(0, ClipId(1))])).is_empty());
        assert!(selected_linked_clip_ids(&visuals, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn selected_linked_clip_ids_collects_partners_of_every_selected_clip() {
        // Due coppie collegate indipendenti, entrambe selezionate: le
        // gemelle di *entrambe* vanno evidenziate.
        let visuals = vec![
            visual_linked(0, 1, 0, 10, 2),
            visual_linked(1, 2, 0, 10, 1),
            visual_linked(0, 3, 20, 10, 4),
            visual_linked(1, 4, 20, 10, 3),
        ];
        let selected = BTreeSet::from([(0, ClipId(1)), (0, ClipId(3))]);
        assert_eq!(
            selected_linked_clip_ids(&visuals, &selected),
            HashSet::from([ClipId(2), ClipId(4)])
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
        let rect = clip_local_rect(&visuals[0], 1.0).union(clip_local_rect(&visuals[2], 1.0));
        let hits: BTreeSet<_> = clips_intersecting_rect(&visuals, 1.0, rect)
            .into_iter()
            .collect();
        assert_eq!(hits, BTreeSet::from([(0, ClipId(1)), (1, ClipId(3))]));
    }

    #[test]
    fn neighbor_bounds_no_neighbors_is_unbounded() {
        let visuals = vec![visual(0, 1, 10, 5)];
        assert_eq!(neighbor_bounds(&visuals, 0, ClipId(1)), (0, FrameIdx::MAX));
    }

    #[test]
    fn neighbor_bounds_clamped_by_prev_and_next_on_same_track() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // finisce a 10
            visual(0, 2, 20, 30), // il moving
            visual(0, 3, 50, 5),  // inizia a 50
            visual(1, 4, 15, 3),  // altra track: ignorata
        ];
        assert_eq!(neighbor_bounds(&visuals, 0, ClipId(2)), (10, 50));
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
    fn combined_drag_range_unlinked_matches_plain_drag_range() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 30)];
        let (min, max, linked) = combined_drag_range(&visuals, 0, ClipId(2), None);
        assert_eq!((min, max), drag_range(&visuals, 0, ClipId(2)));
        assert!(linked.is_none());
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
        let (min, max, linked) = combined_drag_range(&visuals, 0, ClipId(2), Some(ClipId(3)));
        assert_eq!(min, 10);
        assert_eq!(max, 25);
        assert_eq!(linked, Some((ClipId(3), 1, 0)));
    }

    #[test]
    fn combined_drag_range_respects_nonzero_offset_between_linked_clips() {
        // La gemella non è allineata: parte 5 frame dopo la primaria.
        let visuals = vec![
            visual(0, 1, 10, 20), // primaria, track 0, start=10
            visual(1, 2, 15, 20), // gemella, track 1, start=15 (offset=5)
            visual(1, 3, 60, 5),  // vincola la gemella: max_start <= 60-20=40
        ];
        let (_, max, linked) = combined_drag_range(&visuals, 0, ClipId(1), Some(ClipId(2)));
        // vincolo gemella tradotto: primaria.max_start <= 40 - offset(5) = 35
        assert_eq!(max, 35);
        assert_eq!(linked, Some((ClipId(2), 1, 5)));
    }

    #[test]
    fn snap_frame_snaps_start_to_nearby_clip_end() {
        // Clip esistente [0,10): il suo bordo di fine è 10. Un candidato a
        // 12 (entro soglia) deve agganciarsi esattamente lì.
        let visuals = vec![visual(0, 1, 0, 10)];
        let px_per_frame = 5.0; // soglia 10px / 5px_per_frame = 2 frame
        let snapped = snap_frame(12, 20, &visuals, &[], px_per_frame, true);
        assert_eq!(snapped, 10);
    }

    #[test]
    fn snap_frame_snaps_end_of_dragged_clip_to_nearby_clip_start() {
        // Clip esistente [50,60): la clip trascinata (lunga 20) deve
        // agganciare la propria *fine* a 50, cioè candidate_start=30.
        let visuals = vec![visual(0, 1, 50, 10)];
        let snapped = snap_frame(32, 20, &visuals, &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_ignores_clips_beyond_threshold() {
        let visuals = vec![visual(0, 1, 0, 10)];
        // 20 frame di distanza dal bordo (10): a px_per_frame=5.0 la soglia
        // è di soli 2 frame, quindi resta invariato.
        let snapped = snap_frame(30, 5, &visuals, &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_disabled_is_a_no_op() {
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[], 5.0, false);
        assert_eq!(snapped, 12);
    }

    #[test]
    fn snap_frame_excludes_given_clip_ids() {
        // La clip 1 sarebbe un aggancio valido, ma è esclusa (è la clip
        // stessa che si sta trascinando, o la sua gemella collegata).
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[ClipId(1)], 5.0, true);
        assert_eq!(snapped, 12);
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
            let clip = Clip {
                id: project.alloc_clip_id(),
                source: vv_core::ClipSource::SolidColor,
                source_in: 0,
                source_out: len,
                timeline_start: start,
                effects: vv_core::EffectStack::default(),
                linked: None,
            };
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
                );
            });
        });
        // Il font atlas genera una texture delta: va consumata esplicitamente
        // o egui panica al drop (diagnostica pensata per un vero renderer).
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);
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
        let clip = Clip {
            id: project.alloc_clip_id(),
            source: vv_core::ClipSource::SolidColor,
            source_in: 0,
            source_out: 10,
            timeline_start: 0,
            effects: vv_core::EffectStack::default(),
            linked: None,
        };
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
