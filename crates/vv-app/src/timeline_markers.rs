//! Markers in the lane under the ruler ticks: drawing, hover note, click,
//! drag (Alt+drag stretches a single-frame marker into a range) and the
//! note editor.

use vv_core::{ClipColor, FrameIdx, Marker, MarkerId, Project, TimelineId};

use super::{
    COLOR_GRID_COLUMNS, ClipVisual, MARKER_LANE_HEIGHT, PendingAction, RULER_TICKS_HEIGHT,
    TimelineState, clip_color_swatch, drop_outline, palette_color, snap_frame,
};

const MARKER_RADIUS: f32 = 4.5;
/// Half width of the grab area of a single-frame marker.
const POINT_HIT_HALF_WIDTH: f32 = 7.0;
/// Distance (px) from the ends of a range marker that resizes it.
const RANGE_EDGE_HIT_PX: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Move,
    Start,
    End,
    /// Alt+drag: one end stays on the grabbed marker's start, the other
    /// follows the pointer.
    Extend,
}

pub(super) struct MarkerDrag {
    mode: DragMode,
    original: Marker,
    grab_frame: FrameIdx,
    preview: Marker,
}

pub(super) struct MarkerEditor {
    timeline_id: TimelineId,
    pub(super) id: MarkerId,
    pub(super) note: String,
    pub(super) color: ClipColor,
    anchor: egui::Pos2,
    focus_requested: bool,
}

pub(super) fn lane_rect(origin: egui::Pos2, content_width: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        origin + egui::vec2(0.0, RULER_TICKS_HEIGHT),
        egui::vec2(content_width, MARKER_LANE_HEIGHT),
    )
}

pub(super) enum MarkerChange {
    AddAtPlayhead,
    Moved(Marker),
    Edited(Marker),
    Removed(MarkerId),
}

pub(super) fn apply_change(
    project: &mut Project,
    history: &mut vv_core::History,
    timeline_id: TimelineId,
    playhead: FrameIdx,
    change: MarkerChange,
) {
    let command = match change {
        MarkerChange::AddAtPlayhead => {
            return add_marker_at_playhead(project, history, timeline_id, playhead);
        }
        MarkerChange::Moved(marker) => vv_core::SetMarker::moved(timeline_id, marker),
        MarkerChange::Edited(marker) => vv_core::SetMarker::edit(timeline_id, marker),
        MarkerChange::Removed(id) => vv_core::SetMarker::remove(timeline_id, id),
    };
    history.do_command(project, Box::new(command));
}

/// Adds a single-frame marker at the playhead, unless one already starts there.
pub(crate) fn add_marker_at_playhead(
    project: &mut Project,
    history: &mut vv_core::History,
    timeline_id: TimelineId,
    playhead: FrameIdx,
) {
    let timeline = &project.timelines[timeline_id];
    if timeline.markers.iter().any(|m| m.start == playhead) {
        return;
    }
    let marker = Marker {
        id: timeline.alloc_marker_id(),
        start: playhead,
        duration: 0,
        note: String::new(),
        color: Marker::default_color(),
    };
    history.do_command(
        project,
        Box::new(vv_core::SetMarker::add(timeline_id, marker)),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn show_markers(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    origin: egui::Pos2,
    content_width: f32,
    timeline_id: TimelineId,
    markers: &[Marker],
    state: &mut TimelineState,
    visuals: &[ClipVisual],
    px_per_frame: f32,
    snapping_enabled: bool,
    pending: &mut Option<PendingAction>,
) {
    let lane = lane_rect(origin, content_width);
    #[cfg(test)]
    {
        state.marker_lane = lane;
    }
    let x_of = |frame: FrameIdx| origin.x + frame as f32 * px_per_frame;
    let frame_at = |x: f32| ((x - origin.x) / px_per_frame).round() as FrameIdx;
    let snap = |frame: FrameIdx, len: FrameIdx| {
        snap_frame(
            frame,
            len,
            visuals,
            &[],
            &[state.playhead],
            px_per_frame,
            snapping_enabled,
        )
    };
    let mut drag_finished = false;
    let mut new_drag = None;
    let mut new_playhead = None;
    let mut open_editor = None;

    for marker in markers {
        let shown = match &state.marker_drag {
            Some(d) if d.original.id == marker.id => d.preview.clone(),
            _ => marker.clone(),
        };
        let (x0, x1) = (x_of(shown.start), x_of(shown.end()));
        let hit = egui::Rect::from_x_y_ranges(
            (x0 - POINT_HIT_HALF_WIDTH)
                ..=(x1 + POINT_HIT_HALF_WIDTH).max(x0 + POINT_HIT_HALF_WIDTH),
            lane.y_range(),
        );
        let resp = ui.interact(
            hit,
            ui.id().with(("timeline_marker", marker.id)),
            egui::Sense::click_and_drag(),
        );
        let edge_at = |x: f32| {
            if shown.duration == 0 || x1 - x0 < 3.0 * RANGE_EDGE_HIT_PX {
                None
            } else if (x - x0).abs() <= RANGE_EDGE_HIT_PX {
                Some(DragMode::Start)
            } else if (x - x1).abs() <= RANGE_EDGE_HIT_PX {
                Some(DragMode::End)
            } else {
                None
            }
        };

        if resp.drag_started()
            && let Some(press) = ui.input(|i| i.pointer.press_origin())
        {
            let alt = ui.input(|i| i.modifiers.alt);
            let mode = match edge_at(press.x) {
                Some(edge) => edge,
                None if alt && marker.duration == 0 => DragMode::Extend,
                None => DragMode::Move,
            };
            new_drag = Some(MarkerDrag {
                mode,
                original: marker.clone(),
                grab_frame: frame_at(press.x),
                preview: marker.clone(),
            });
        } else if resp.dragged()
            && let Some(drag) = state
                .marker_drag
                .as_ref()
                .filter(|d| d.original.id == marker.id)
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let preview = drag_preview(drag, frame_at(pos.x), snap);
            if let Some(d) = &mut state.marker_drag {
                d.preview = preview;
            }
        } else if resp.drag_stopped() {
            drag_finished = true;
            if let Some(d) = &state.marker_drag
                && d.original.id == marker.id
                && d.preview != d.original
            {
                *pending = Some(PendingAction::Marker(MarkerChange::Moved(
                    d.preview.clone(),
                )));
            }
        }
        if resp.double_clicked() {
            open_editor = Some((marker, egui::pos2(x0, lane.bottom() + 4.0)));
        } else if resp.clicked() {
            new_playhead = Some(marker.start);
        }
        resp.context_menu(|ui| {
            if ui.button(t!("timeline.edit_marker")).clicked() {
                open_editor = Some((marker, egui::pos2(x0, lane.bottom() + 4.0)));
                ui.close();
            }
            if ui.button(t!("timeline.delete_marker")).clicked() {
                *pending = Some(PendingAction::Marker(MarkerChange::Removed(marker.id)));
                ui.close();
            }
        });

        let hover_pos = resp.hover_pos();
        if let Some(pos) = hover_pos
            && state.marker_drag.is_none()
        {
            let icon = match edge_at(pos.x) {
                Some(_) => egui::CursorIcon::ResizeHorizontal,
                None => egui::CursorIcon::Grab,
            };
            ui.ctx().set_cursor_icon(icon);
        }
        if state.marker_drag.is_none() && !marker.note.is_empty() {
            resp.on_hover_text(&marker.note);
        }

        let highlighted = state
            .marker_drag
            .as_ref()
            .is_some_and(|d| d.original.id == marker.id)
            || state
                .marker_editor
                .as_ref()
                .is_some_and(|e| e.id == marker.id);
        paint_marker(painter, lane, x0, x1, shown.color, highlighted);
    }

    if let Some(drag) = new_drag {
        state.marker_drag = Some(drag);
    }
    // Cleared after the loop: markers drawn later in this frame still read it.
    if drag_finished {
        state.marker_drag = None;
    }
    if let Some(frame) = new_playhead {
        state.playhead = frame;
    }
    if let Some((marker, anchor)) = open_editor {
        state.marker_editor = Some(MarkerEditor {
            timeline_id,
            id: marker.id,
            note: marker.note.clone(),
            color: marker.color,
            anchor,
            focus_requested: false,
        });
    }
    show_editor(ui.ctx(), timeline_id, markers, state, pending);
}

fn drag_preview(
    drag: &MarkerDrag,
    pointer_frame: FrameIdx,
    snap: impl Fn(FrameIdx, FrameIdx) -> FrameIdx,
) -> Marker {
    let original = &drag.original;
    let mut preview = original.clone();
    match drag.mode {
        DragMode::Move => {
            let start = original.start + pointer_frame - drag.grab_frame;
            preview.start = snap(start, original.duration).max(0);
        }
        DragMode::Start => {
            let start = snap(pointer_frame, 0).clamp(0, original.end());
            preview.start = start;
            preview.duration = original.end() - start;
        }
        DragMode::End => {
            let end = snap(pointer_frame, 0).max(original.start);
            preview.duration = end - original.start;
        }
        DragMode::Extend => {
            let other = snap(pointer_frame, 0).max(0);
            preview.start = original.start.min(other);
            preview.duration = (other - original.start).abs();
        }
    }
    preview
}

fn paint_marker(
    painter: &egui::Painter,
    lane: egui::Rect,
    x0: f32,
    x1: f32,
    color: ClipColor,
    highlighted: bool,
) {
    let fill = palette_color(color);
    if x1 > x0 {
        let band = egui::Rect::from_x_y_ranges(x0..=x1, (lane.top() + 3.0)..=(lane.bottom() - 1.0));
        painter.rect_filled(band, 2.0, fill.gamma_multiply(0.35));
        painter.hline(band.x_range(), band.bottom(), egui::Stroke::new(2.0, fill));
        painter.vline(x1, band.y_range(), egui::Stroke::new(1.5, fill));
    }
    // Upside-down drop: the tip rests on the marked frame.
    let center = egui::pos2(x0, lane.center().y);
    let outline = drop_outline(center, MARKER_RADIUS)
        .into_iter()
        .map(|p| egui::pos2(p.x, 2.0 * center.y - p.y))
        .collect();
    let stroke = if highlighted {
        egui::Stroke::new(1.5, egui::Color32::WHITE)
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(140))
    };
    painter.add(egui::Shape::convex_polygon(outline, fill, stroke));
}

fn show_editor(
    ctx: &egui::Context,
    timeline_id: TimelineId,
    markers: &[Marker],
    state: &mut TimelineState,
    pending: &mut Option<PendingAction>,
) {
    let Some(editor) = &mut state.marker_editor else {
        return;
    };
    // Undo may have removed it, or another timeline is now shown.
    let marker = markers.iter().find(|m| m.id == editor.id);
    let Some(marker) = marker.filter(|_| editor.timeline_id == timeline_id) else {
        state.marker_editor = None;
        return;
    };
    let mut open = true;
    let mut save = false;
    let mut delete = false;
    let window = egui::Window::new(t!("timeline.marker"))
        .id(egui::Id::new("timeline_marker_editor"))
        .collapsible(false)
        .resizable(false)
        .fixed_pos(editor.anchor)
        .open(&mut open)
        .show(ctx, |ui| {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
                save = true;
            }
            let note = ui.add(
                egui::TextEdit::multiline(&mut editor.note)
                    .desired_rows(3)
                    .desired_width(220.0)
                    .hint_text(t!("timeline.marker_note_hint")),
            );
            if !editor.focus_requested {
                note.request_focus();
                editor.focus_requested = true;
            }
            egui::Grid::new("marker_color_grid")
                .spacing(egui::vec2(2.0, 2.0))
                .show(ui, |ui| {
                    for (i, color) in ClipColor::ALL.into_iter().enumerate() {
                        if clip_color_swatch(ui, color, editor.color == color).clicked() {
                            editor.color = color;
                        }
                        if (i + 1) % COLOR_GRID_COLUMNS == 0 {
                            ui.end_row();
                        }
                    }
                });
            ui.horizontal(|ui| {
                if ui.button(t!("timeline.marker_save")).clicked() {
                    save = true;
                }
                if ui.button(t!("timeline.delete_marker")).clicked() {
                    delete = true;
                }
            });
            ui.weak(t!("timeline.marker_editor_hint"));
        });
    // Ctrl+click anywhere in the editor (a swatch included, already applied
    // above) saves and closes.
    if let Some(window) = window
        && ctx.input(|i| {
            i.modifiers.command
                && i.pointer.primary_clicked()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| window.response.rect.contains(p))
        })
    {
        save = true;
    }
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        open = false;
    }

    if delete {
        *pending = Some(PendingAction::Marker(MarkerChange::Removed(marker.id)));
        state.marker_editor = None;
    } else if save {
        if editor.note != marker.note || editor.color != marker.color {
            let edited = Marker {
                note: editor.note.clone(),
                color: editor.color,
                ..marker.clone()
            };
            *pending = Some(PendingAction::Marker(MarkerChange::Edited(edited)));
        }
        state.marker_editor = None;
    } else if !open {
        state.marker_editor = None;
    }
}

#[cfg(test)]
#[path = "tests/timeline_markers.rs"]
mod tests;
