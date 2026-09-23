//! Viewer zoom and pan: wheel zooms around the pointer; middle button,
//! Ctrl+wheel (vertical) and Ctrl+Shift+wheel (horizontal) pan.

use egui::{Pos2, Rect, Vec2};

pub const PRESETS: [f32; 8] = [0.0625, 0.125, 0.25, 0.5, 0.75, 1.0, 2.0, 3.0];
const MIN_SCALE: f32 = 0.02;
const MAX_SCALE: f32 = 32.0;
/// Frame still visible when panned to the edge, in points.
const MIN_VISIBLE: f32 = 32.0;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ViewerZoom {
    /// Frame pixels per physical screen pixel; `None` fits the frame to the viewer.
    scale: Option<f32>,
    /// Frame center relative to the viewer center, in points.
    pan: Vec2,
    panning: bool,
}

impl ViewerZoom {
    pub fn fit(&mut self) {
        *self = Self::default();
    }

    pub fn is_fit(&self) -> bool {
        self.scale.is_none()
    }

    /// Keeps the point at the viewer center where it is.
    pub fn set_scale(&mut self, scale: f32, area: Rect, frame_px: Vec2, ppp: f32) {
        let old = self.scale(area, frame_px, ppp);
        let new = scale.clamp(MIN_SCALE, MAX_SCALE);
        if old > 0.0 {
            self.pan *= new / old;
        }
        self.scale = Some(new);
        self.clamp_pan(area, frame_px, ppp);
    }

    pub fn scale(&self, area: Rect, frame_px: Vec2, ppp: f32) -> f32 {
        self.scale.unwrap_or_else(|| fit_scale(area, frame_px, ppp))
    }

    /// Ui for the tools over the frame (transform handles, drag to the
    /// timeline): while the player pans they stay drawn but get no pointer,
    /// so the middle button is the player's only.
    pub fn tools_ui(&self, ui: &mut egui::Ui, area: Rect) -> egui::Ui {
        let mut builder = egui::UiBuilder::new().max_rect(area);
        if self.panning {
            builder = builder.disabled();
        }
        let opacity = ui.opacity();
        let mut tools = ui.new_child(builder);
        // `disabled` also fades the painting.
        tools.set_opacity(opacity);
        tools
    }

    pub fn frame_rect(&self, area: Rect, frame_px: Vec2, ppp: f32) -> Rect {
        let size = frame_px * self.scale(area, frame_px, ppp) / ppp;
        Rect::from_center_size(area.center() + self.pan, size)
    }

    pub fn handle_input(&mut self, ui: &egui::Ui, area: Rect, frame_px: Vec2) {
        let ppp = ui.ctx().pixels_per_point();
        let hovered = ui.rect_contains_pointer(area);
        let line_speed = ui.ctx().options(|o| o.input_options.line_scroll_speed);
        let (factor, wheel_pan, pointer, middle_pressed, middle_down, delta) = ui.input(|i| {
            // egui turns Ctrl+wheel into `zoom_delta`: here it pans instead,
            // so only the pinch gestures zoom.
            let (factor, wheel_pan) = if i.modifiers.command {
                (pinch_factor(i), ctrl_wheel_pan(i, line_speed))
            } else {
                ((i.smooth_scroll_delta.y / 200.0).exp() * i.zoom_delta(), Vec2::ZERO)
            };
            (
                factor,
                wheel_pan,
                i.pointer.hover_pos(),
                i.pointer.button_pressed(egui::PointerButton::Middle),
                i.pointer.button_down(egui::PointerButton::Middle),
                i.pointer.delta(),
            )
        });
        if hovered && middle_pressed {
            self.panning = true;
        }
        if !middle_down {
            self.panning = false;
        }
        if self.panning && delta != Vec2::ZERO {
            self.scale = Some(self.scale(area, frame_px, ppp));
            self.pan += delta;
            self.clamp_pan(area, frame_px, ppp);
        }
        if hovered && !self.is_fit() && wheel_pan != Vec2::ZERO {
            self.pan += wheel_pan;
            self.clamp_pan(area, frame_px, ppp);
        }
        if hovered
            && factor != 1.0
            && let Some(pointer) = pointer
        {
            self.zoom_around(pointer, factor, area, frame_px, ppp);
        }
    }

    fn zoom_around(&mut self, pointer: Pos2, factor: f32, area: Rect, frame_px: Vec2, ppp: f32) {
        let old = self.scale(area, frame_px, ppp);
        if old <= 0.0 {
            return;
        }
        let new = (old * factor).clamp(MIN_SCALE, MAX_SCALE);
        let center = area.center() + self.pan;
        let new_center = pointer + (center - pointer) * (new / old);
        self.scale = Some(new);
        self.pan = new_center - area.center();
        self.clamp_pan(area, frame_px, ppp);
    }

    fn clamp_pan(&mut self, area: Rect, frame_px: Vec2, ppp: f32) {
        let size = frame_px * self.scale(area, frame_px, ppp) / ppp;
        let limit = ((size + area.size()) / 2.0 - Vec2::splat(MIN_VISIBLE)).max(Vec2::ZERO);
        self.pan = self.pan.clamp(-limit, limit);
    }
}

fn pinch_factor(input: &egui::InputState) -> f32 {
    if let Some(touch) = input.multi_touch() {
        return touch.zoom_delta;
    }
    input
        .events
        .iter()
        .filter_map(|e| match e {
            egui::Event::Zoom(factor) => Some(*factor),
            _ => None,
        })
        .product()
}

/// Pan in points from the wheel events of this frame: vertical, or
/// horizontal with Shift.
fn ctrl_wheel_pan(input: &egui::InputState, line_speed: f32) -> Vec2 {
    let mut pan = Vec2::ZERO;
    for event in &input.events {
        if let egui::Event::MouseWheel { unit, delta, .. } = event {
            let points = match unit {
                egui::MouseWheelUnit::Point => *delta,
                egui::MouseWheelUnit::Line => *delta * line_speed,
                egui::MouseWheelUnit::Page => *delta * input.viewport_rect().height(),
            };
            pan += points;
        }
    }
    if input.modifiers.shift {
        // Some platforms already report Shift+wheel as horizontal.
        egui::vec2(pan.x + pan.y, 0.0)
    } else {
        pan
    }
}

fn fit_scale(area: Rect, frame_px: Vec2, ppp: f32) -> f32 {
    if frame_px.x <= 0.0 || frame_px.y <= 0.0 {
        return 0.0;
    }
    (area.width() / frame_px.x).min(area.height() / frame_px.y).max(0.0) * ppp
}

/// `352%`, `6.25%`.
pub fn percent_label(scale: f32) -> String {
    let percent = scale * 100.0;
    if percent >= 100.0 {
        format!("{percent:.0}%")
    } else {
        let text = format!("{percent:.2}");
        format!("{}%", text.trim_end_matches('0').trim_end_matches('.'))
    }
}

#[cfg(test)]
#[path = "tests/viewer_zoom.rs"]
mod tests;
