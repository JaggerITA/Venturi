//! Widget timeline multi-traccia: disegna tracce/clip, gestisce selezione,
//! drag orizzontale (con clip collegate che si muovono insieme, vedi
//! `Clip::linked`), menu contestuale per collegare/scollegare, e il
//! playhead. Ogni mutazione passa da `History::do_command`, mai da una
//! modifica diretta del `Project`.
//!
//! Disegno "immediate mode" a basso livello (painter diretto, non widget
//! egui nidificati): per una griglia densa di rettangoli come una timeline
//! dà più controllo e meno overhead dei container annidati.

use vv_core::{Clip, ClipId, FrameIdx, History, Project, TimelineId, Track, TrackKind};

const ROW_HEIGHT: f32 = 40.0;
const RULER_HEIGHT: f32 = 20.0;
const MIN_TIMELINE_SECS: f64 = 20.0;
const TRAILING_MARGIN_SECS: f64 = 5.0;

pub struct TimelineState {
    pub selected: Option<(usize, ClipId)>,
    pub playhead: FrameIdx,
    pixels_per_sec: f32,
    drag: Option<DragState>,
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
            selected: None,
            playhead: 0,
            pixels_per_sec: 60.0,
            drag: None,
        }
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
/// conflitto di borrow con `history.do_command(project, ...)`.
enum PendingAction {
    /// (clip_id, track_index, new_start) per una o due clip (collegate).
    Move(Vec<(ClipId, usize, FrameIdx)>),
    Select(usize, ClipId),
    ClearSelection,
    Unlink(usize, ClipId),
    Link(usize, ClipId, usize, ClipId),
}

pub fn show_timeline(
    ui: &mut egui::Ui,
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
    state: &mut TimelineState,
) {
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
        .show(ui, |ui| {
            let (rect, _resp) = ui.allocate_exact_size(
                egui::vec2(content_width, content_height),
                egui::Sense::hover(),
            );
            let painter = ui.painter_at(rect);
            let origin = rect.min;

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

            // Sfondo delle track (alternato per leggibilità).
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

            // Posizione (clampata) della clip primaria in trascinamento,
            // calcolata una sola volta e riusata sia per lei sia per
            // l'eventuale gemella collegata.
            let dragged_primary_new_start = state.drag.as_ref().map(|d| {
                let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                (raw.round() as FrameIdx).clamp(d.min_start, d.max_start)
            });

            // La gemella collegata della clip selezionata va evidenziata
            // insieme a lei (le clip audio+video sono collegate di
            // default): calcolato una volta sola, non per ogni clip.
            let selected_linked_id = selected_linked_clip_id(&visuals, state.selected);

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

                let is_selected = state.selected == Some((visual.track_index, visual.clip.id))
                    || selected_linked_id == Some(visual.clip.id);
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
                    painter.text(
                        clip_rect.right_top() + egui::vec2(-4.0, 2.0),
                        egui::Align2::RIGHT_TOP,
                        "🔗",
                        egui::FontId::proportional(12.0),
                        egui::Color32::BLACK,
                    );
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
                        let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                        let new_start = (raw.round() as FrameIdx).clamp(d.min_start, d.max_start);
                        let mut moves = vec![(d.clip_id, d.track_index, new_start)];
                        if let Some((partner_id, partner_track, offset)) = d.linked {
                            moves.push((partner_id, partner_track, new_start + offset));
                        }
                        pending = Some(PendingAction::Move(moves));
                    }
                } else if resp.clicked() {
                    pending = Some(PendingAction::Select(visual.track_index, visual.clip.id));
                }

                resp.context_menu(|ui| {
                    if visual.clip.linked.is_some() {
                        if ui.button("Scollega audio/video").clicked() {
                            pending =
                                Some(PendingAction::Unlink(visual.track_index, visual.clip.id));
                            ui.close();
                        }
                    } else {
                        let candidate = visuals.iter().find(|v| {
                            v.clip.id != visual.clip.id
                                && v.track_index != visual.track_index
                                && v.clip.linked.is_none()
                                && v.clip.timeline_start == visual.clip.timeline_start
                        });
                        match candidate {
                            Some(candidate) => {
                                if ui
                                    .button(format!("Collega con \"{}\"", candidate.label))
                                    .clicked()
                                {
                                    pending = Some(PendingAction::Link(
                                        visual.track_index,
                                        visual.clip.id,
                                        candidate.track_index,
                                        candidate.clip.id,
                                    ));
                                    ui.close();
                                }
                            }
                            None => {
                                ui.label("Nessuna clip allineata da collegare");
                            }
                        }
                    }
                });
            }

            if ruler_resp.clicked() {
                pending = Some(PendingAction::ClearSelection);
            }

            // Playhead.
            let px = origin.x + state.playhead as f32 * px_per_frame;
            painter.line_segment(
                [
                    egui::pos2(px, origin.y),
                    egui::pos2(px, origin.y + content_height),
                ],
                egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 50, 50)),
            );
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
            PendingAction::Select(track_index, clip_id) => {
                state.selected = Some((track_index, clip_id));
            }
            PendingAction::ClearSelection => {
                state.selected = None;
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

/// L'id della gemella collegata della clip attualmente selezionata, se
/// c'è: usato per evidenziarla insieme alla selezione (bug: "seleziono il
/// video ma non l'audio collegato").
fn selected_linked_clip_id(
    visuals: &[ClipVisual],
    selected: Option<(usize, ClipId)>,
) -> Option<ClipId> {
    let (track_index, clip_id) = selected?;
    visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
        .and_then(|v| v.clip.linked)
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
    fn selected_linked_clip_id_finds_the_partner() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 2), visual_linked(1, 2, 0, 10, 1)];
        assert_eq!(
            selected_linked_clip_id(&visuals, Some((0, ClipId(1)))),
            Some(ClipId(2)),
            "selezionando il video deve trovare l'audio collegato"
        );
        assert_eq!(
            selected_linked_clip_id(&visuals, Some((1, ClipId(2)))),
            Some(ClipId(1)),
            "e viceversa, selezionando l'audio deve trovare il video"
        );
    }

    #[test]
    fn selected_linked_clip_id_is_none_when_unlinked_or_unselected() {
        let visuals = vec![visual(0, 1, 0, 10)];
        assert_eq!(
            selected_linked_clip_id(&visuals, Some((0, ClipId(1)))),
            None
        );
        assert_eq!(selected_linked_clip_id(&visuals, None), None);
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
            selected: Some((0, ClipId(0))),
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
                );
            });
        });
        // Il font atlas genera una texture delta: va consumata esplicitamente
        // o egui panica al drop (diagnostica pensata per un vero renderer).
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);
    }
}
