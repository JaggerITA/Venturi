//! Position, scale and anchor point handles of the selected clip, drawn on
//! top of the viewer. Same geometry as the shader (`transform.wgsl`): a
//! point `p` of the clip (timeline pixels from its center, Y up) ends up at
//! `position + anchor + R(zoom * (p - anchor))`, with `R` a clockwise
//! rotation.

use vv_core::Transform;

const HANDLE_RADIUS: f32 = 4.5;
const ANCHOR_RADIUS: f32 = 6.5;
/// On-screen distance of the rotation knob from the pivot.
const ROTATION_ARM: f32 = 100.0;
/// Rotation step with Shift held.
const ROTATION_SNAP: f32 = 15.0;
const MIN_ZOOM: f32 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Handle {
    Move,
    Anchor,
    Rotate,
    /// Sign of the scaled axes: (±1, ±1) a corner, (±1, 0)/(0, ±1) a side.
    Scale(f32, f32),
}

pub struct OverlayDrag {
    handle: Handle,
    start_pointer: egui::Pos2,
    start: Transform,
    last_pointer: egui::Pos2,
    /// Degrees turned so far with the rotation knob.
    turned: f32,
}

/// Where the clip sits in the frame, without transform: the source fitted
/// into the timeline, minus the crop.
#[derive(Debug, Clone, Copy)]
struct ClipBox {
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
}

impl ClipBox {
    fn new(timeline_size: (u32, u32), source_size: (u32, u32), crop: [f32; 4]) -> Self {
        let (tw, th) = (timeline_size.0.max(1) as f32, timeline_size.1.max(1) as f32);
        let (sw, sh) = (source_size.0.max(1) as f32, source_size.1.max(1) as f32);
        let fit = (tw / sw).min(th / sh);
        let (w, h) = (sw * fit, sh * fit);
        Self {
            left: -w / 2.0 + crop[0] * fit,
            right: w / 2.0 - crop[2] * fit,
            top: h / 2.0 - crop[1] * fit,
            bottom: -h / 2.0 + crop[3] * fit,
        }
    }

    /// Point of the clip for a scale handle: corner or midpoint of a side.
    fn point(&self, sx: f32, sy: f32) -> [f32; 2] {
        let pick = |s: f32, lo: f32, hi: f32| match s {
            s if s < 0.0 => lo,
            s if s > 0.0 => hi,
            _ => (lo + hi) / 2.0,
        };
        [
            pick(sx, self.left, self.right),
            pick(sy, self.bottom, self.top),
        ]
    }
}

fn rotate_cw(v: [f32; 2], degrees: f32) -> [f32; 2] {
    let (sn, cs) = degrees.to_radians().sin_cos();
    [v[0] * cs + v[1] * sn, -v[0] * sn + v[1] * cs]
}

/// From a point of the clip to the frame (timeline pixels from the center, Y up).
fn clip_to_frame(t: &Transform, p: [f32; 2]) -> [f32; 2] {
    let scaled = [
        t.zoom[0] * (p[0] - t.anchor[0]),
        t.zoom[1] * (p[1] - t.anchor[1]),
    ];
    let r = rotate_cw(scaled, t.rotation);
    [
        t.position[0] + t.anchor[0] + r[0],
        t.position[1] + t.anchor[1] + r[1],
    ]
}

/// The new transform from dragging `handle` by `delta` (timeline pixels,
/// Y up) starting from `start`. `free`: a corner scales the two axes
/// independently instead of proportionally.
fn drag_transform(
    start: &Transform,
    clip_box: &ClipBox,
    handle: Handle,
    delta: [f32; 2],
    free: bool,
) -> Transform {
    let mut t = *start;
    match handle {
        Handle::Rotate => {}
        Handle::Move => {
            t.position = [start.position[0] + delta[0], start.position[1] + delta[1]];
        }
        // As from the panel: only the anchor changes, and the on-screen pivot
        // (`position + anchor`) follows the pointer.
        Handle::Anchor => {
            t.anchor = [start.anchor[0] + delta[0], start.anchor[1] + delta[1]];
        }
        Handle::Scale(sx, sy) => {
            let h = clip_box.point(sx, sy);
            // Handle relative to the anchor, in the unrotated frame of reference.
            let from = [
                start.zoom[0] * (h[0] - start.anchor[0]),
                start.zoom[1] * (h[1] - start.anchor[1]),
            ];
            let moved = rotate_cw(delta, -start.rotation);
            let to = [from[0] + moved[0], from[1] + moved[1]];
            let ratio = |axis: usize| (from[axis].abs() > 1e-3).then(|| to[axis] / from[axis]);
            let uniform = sx != 0.0 && sy != 0.0 && !free;
            if uniform {
                let len2 = from[0] * from[0] + from[1] * from[1];
                if len2 > 1e-6 {
                    let k = (to[0] * from[0] + to[1] * from[1]) / len2;
                    t.zoom = [
                        (start.zoom[0] * k).max(MIN_ZOOM),
                        (start.zoom[1] * k).max(MIN_ZOOM),
                    ];
                }
            } else {
                for (axis, s) in [(0, sx), (1, sy)] {
                    if s != 0.0
                        && let Some(k) = ratio(axis)
                    {
                        t.zoom[axis] = (start.zoom[axis] * k).max(MIN_ZOOM);
                    }
                }
            }
        }
    }
    t
}

/// Clockwise angle (degrees) taking `from` onto `to`, vectors from the pivot
/// with Y up, in the range (-180, 180].
fn clockwise_turn(from: [f32; 2], to: [f32; 2]) -> f32 {
    let d = (from[1].atan2(from[0]) - to[1].atan2(to[0])).to_degrees();
    (d + 180.0).rem_euclid(360.0) - 180.0
}

/// Draws the handles on top of `frame_rect` (where the viewer shows the
/// timeline frame) and handles the dragging, which can start anywhere in
/// `area`: the handles can fall outside the frame. Returns the new transform
/// if the user changed it in this frame.
pub fn show(
    ui: &egui::Ui,
    frame_rect: egui::Rect,
    area: egui::Rect,
    timeline_size: (u32, u32),
    source_size: (u32, u32),
    transform: &Transform,
    drag: &mut Option<OverlayDrag>,
) -> Option<Transform> {
    let scale = frame_rect.width() / timeline_size.0.max(1) as f32;
    let to_screen = |p: [f32; 2]| frame_rect.center() + egui::vec2(p[0] * scale, -p[1] * scale);
    let clip_box = ClipBox::new(timeline_size, source_size, transform.crop);

    let screen_of = |sx, sy| to_screen(clip_to_frame(transform, clip_box.point(sx, sy)));
    let pivot = to_screen([
        transform.position[0] + transform.anchor[0],
        transform.position[1] + transform.anchor[1],
    ]);
    let (sn, cs) = transform.rotation.to_radians().sin_cos();
    let knob = pivot + egui::vec2(sn, -cs) * ROTATION_ARM;
    let corners =
        [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)].map(|(sx, sy)| screen_of(sx, sy));
    let scale_handles: Vec<(Handle, egui::Pos2)> = [
        (-1.0, 1.0),
        (1.0, 1.0),
        (1.0, -1.0),
        (-1.0, -1.0),
        (0.0, 1.0),
        (1.0, 0.0),
        (0.0, -1.0),
        (-1.0, 0.0),
    ]
    .into_iter()
    .map(|(sx, sy)| (Handle::Scale(sx, sy), screen_of(sx, sy)))
    .collect();

    let grab = HANDLE_RADIUS + 4.0;
    let handle_at = |pos: egui::Pos2| {
        if pos.distance(pivot) <= ANCHOR_RADIUS + 4.0 {
            return Some(Handle::Anchor);
        }
        if pos.distance(knob) <= grab {
            return Some(Handle::Rotate);
        }
        if let Some((handle, _)) = scale_handles.iter().find(|(_, p)| pos.distance(*p) <= grab) {
            return Some(*handle);
        }
        point_in_convex(pos, &corners).then_some(Handle::Move)
    };

    let resp = ui.interact(
        area,
        ui.id().with("viewer_transform_overlay"),
        egui::Sense::drag(),
    );
    let mut changed = None;
    if resp.drag_started()
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        *drag = handle_at(press).map(|handle| OverlayDrag {
            handle,
            start_pointer: press,
            start: *transform,
            last_pointer: press,
            turned: 0.0,
        });
    }
    if let Some(d) = drag.as_mut()
        && resp.dragged()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let shift = ui.input(|i| i.modifiers.shift);
        let new = if d.handle == Handle::Rotate {
            // Accumulated frame by frame: one can turn past half a revolution.
            let from = d.last_pointer - pivot;
            let to = pos - pivot;
            d.turned += clockwise_turn([from.x, -from.y], [to.x, -to.y]);
            d.last_pointer = pos;
            let mut t = d.start;
            t.rotation = d.start.rotation + d.turned;
            if shift {
                t.rotation = (t.rotation / ROTATION_SNAP).round() * ROTATION_SNAP;
            }
            t
        } else {
            let delta = pos - d.start_pointer;
            drag_transform(
                &d.start,
                &clip_box,
                d.handle,
                [delta.x / scale, -delta.y / scale],
                shift,
            )
        };
        changed = Some(new);
    }
    if resp.drag_stopped() {
        *drag = None;
    }

    let hovered = drag
        .as_ref()
        .map(|d| d.handle)
        .or_else(|| resp.hover_pos().and_then(handle_at));
    if let Some(handle) = hovered {
        ui.ctx().output_mut(|o| {
            o.cursor_icon = match handle {
                Handle::Move => egui::CursorIcon::Move,
                Handle::Anchor => egui::CursorIcon::Crosshair,
                Handle::Rotate if drag.is_some() => egui::CursorIcon::Grabbing,
                Handle::Rotate => egui::CursorIcon::Grab,
                Handle::Scale(0.0, _) => egui::CursorIcon::ResizeVertical,
                Handle::Scale(_, 0.0) => egui::CursorIcon::ResizeHorizontal,
                Handle::Scale(sx, sy) if sx * sy > 0.0 => egui::CursorIcon::ResizeNeSw,
                Handle::Scale(..) => egui::CursorIcon::ResizeNwSe,
            }
        });
    }

    let painter = ui.painter().with_clip_rect(area);
    let accent = crate::theme::ACCENT;
    let line = egui::Stroke::new(1.0, egui::Color32::from_rgb(225, 232, 245));
    let mut outline = corners.to_vec();
    outline.push(corners[0]);
    painter.add(egui::Shape::line(outline, line));
    painter.line_segment([pivot, knob], line);
    let dot = |center: egui::Pos2, radius: f32| {
        painter.circle(
            center,
            radius,
            egui::Color32::WHITE,
            egui::Stroke::new(1.5, accent),
        );
    };
    for (_, p) in &scale_handles {
        dot(*p, HANDLE_RADIUS);
    }
    dot(knob, HANDLE_RADIUS);
    dot(pivot, ANCHOR_RADIUS);

    changed
}

fn point_in_convex(p: egui::Pos2, polygon: &[egui::Pos2]) -> bool {
    let mut sign = 0.0;
    for (i, a) in polygon.iter().enumerate() {
        let b = polygon[(i + 1) % polygon.len()];
        let cross = (b - *a).x * (p - *a).y - (b - *a).y * (p - *a).x;
        if cross != 0.0 {
            if sign != 0.0 && cross.signum() != sign {
                return false;
            }
            sign = cross.signum();
        }
    }
    true
}

#[cfg(test)]
#[path = "tests/viewer_overlay.rs"]
mod tests;
