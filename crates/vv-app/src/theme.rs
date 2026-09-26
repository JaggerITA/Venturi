//! Colors that give the interface its identity, taken from the logo
//! (`media/icons/svg/vv-icon.svg`).

use egui::Color32;

/// The orange of the logo: selection, active toggles, keyframes, playhead head.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x6A, 0x1F);
/// The accent darkened enough to carry light text on top of it.
pub(crate) const ACCENT_FILL: Color32 = Color32::from_rgb(0xA8, 0x45, 0x14);
/// Translucent accent for box selections.
pub(crate) const ACCENT_TRANSLUCENT: Color32 = Color32::from_rgba_premultiplied(40, 17, 5, 40);
/// Red is reserved for errors: media offline, audio clipping.
pub(crate) const ERROR: Color32 = Color32::from_rgb(230, 70, 70);
pub(crate) const PLAYHEAD: Color32 = Color32::from_gray(240);

pub(crate) fn apply(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.visuals.selection.bg_fill = ACCENT_FILL;
            style.visuals.selection.stroke.color = Color32::WHITE;
            style.visuals.hyperlink_color = ACCENT;
        });
    }
}

/// Playhead line with the "T" head of the logo's vertical bar: a bar in the
/// accent across the top, a thicker accent stem down to `head_bottom`.
pub(crate) fn paint_playhead(painter: &egui::Painter, x: f32, top: f32, head_bottom: f32, bottom: f32) {
    const BAR_HALF_WIDTH: f32 = 6.0;
    const BAR_HEIGHT: f32 = 4.0;
    painter.line_segment(
        [egui::pos2(x, head_bottom), egui::pos2(x, bottom)],
        egui::Stroke::new(1.5, PLAYHEAD),
    );
    painter.line_segment(
        [egui::pos2(x, top), egui::pos2(x, head_bottom)],
        egui::Stroke::new(2.5, ACCENT),
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(x - BAR_HALF_WIDTH, top),
            egui::pos2(x + BAR_HALF_WIDTH, top + BAR_HEIGHT),
        ),
        1.5,
        ACCENT,
    );
}
