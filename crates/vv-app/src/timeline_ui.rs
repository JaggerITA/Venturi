//! Widget timeline multi-traccia: disegna tracce/clip, gestisce selezione,
//! drag orizzontale (riposizionamento sulla stessa track — spostare tra
//! track è supportato dal comando `MoveClip` ma non ancora dal drag,
//! milestone successiva) e il playhead. Ogni mutazione passa da
//! `History::do_command`, mai da una modifica diretta del `Project`.
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
    len: FrameIdx,
    accum_px: f32,
    /// Calcolati una sola volta all'inizio del drag, dalla posizione dei
    /// vicini immediati sulla stessa track: la clip non può attraversarli.
    lower_bound: FrameIdx,
    upper_bound: FrameIdx,
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
    Move {
        clip_id: ClipId,
        track_index: usize,
        new_start: FrameIdx,
    },
    Select(usize, ClipId),
    ClearSelection,
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

            // Clip.
            for visual in &visuals {
                let is_dragging_this = state
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.clip_id == visual.clip.id);

                let display_start = if is_dragging_this {
                    let d = state.drag.as_ref().unwrap();
                    let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                    clamp_to_bounds(raw.round() as FrameIdx, d.lower_bound, d.upper_bound, d.len)
                } else {
                    visual.clip.timeline_start
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

                let is_selected = state.selected == Some((visual.track_index, visual.clip.id));
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

                if resp.drag_started() {
                    let (lower_bound, upper_bound) =
                        neighbor_bounds(&visuals, visual.track_index, visual.clip.id);
                    state.drag = Some(DragState {
                        clip_id: visual.clip.id,
                        track_index: visual.track_index,
                        original_start: visual.clip.timeline_start,
                        len: visual.clip.timeline_len(),
                        accum_px: 0.0,
                        lower_bound,
                        upper_bound,
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
                        let new_start = clamp_to_bounds(
                            raw.round() as FrameIdx,
                            d.lower_bound,
                            d.upper_bound,
                            d.len,
                        );
                        pending = Some(PendingAction::Move {
                            clip_id: visual.clip.id,
                            track_index: d.track_index,
                            new_start,
                        });
                    }
                } else if resp.clicked() {
                    pending = Some(PendingAction::Select(visual.track_index, visual.clip.id));
                }
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
            PendingAction::Move {
                clip_id,
                track_index,
                new_start,
            } => {
                history.do_command(
                    project,
                    Box::new(vv_core::MoveClip::new(
                        timeline_id,
                        clip_id,
                        track_index,
                        track_index,
                        new_start,
                    )),
                );
            }
            PendingAction::Select(track_index, clip_id) => {
                state.selected = Some((track_index, clip_id));
            }
            PendingAction::ClearSelection => {
                state.selected = None;
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

fn clamp_to_bounds(start: FrameIdx, lower: FrameIdx, upper: FrameIdx, len: FrameIdx) -> FrameIdx {
    let max_start = upper.saturating_sub(len).max(lower);
    start.clamp(lower, max_start)
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
            },
            label: String::new(),
            color: egui::Color32::WHITE,
        }
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
    fn clamp_to_bounds_keeps_clip_inside_slot() {
        // slot [10, 50), clip lunga 30: può stare solo tra 10 e 20.
        assert_eq!(clamp_to_bounds(0, 10, 50, 30), 10);
        assert_eq!(clamp_to_bounds(15, 10, 50, 30), 15);
        assert_eq!(clamp_to_bounds(100, 10, 50, 30), 20);
    }

    #[test]
    fn clamp_to_bounds_degenerate_slot_does_not_panic() {
        // slot più piccolo della clip: non deve produrre un range invertito.
        let result = clamp_to_bounds(12, 10, 15, 30);
        assert_eq!(result, 10);
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
