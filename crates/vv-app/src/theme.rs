//! Colors that give the interface its identity, taken from the logo
//! (`media/icons/svg/vv-icon.svg`).

use egui::Color32;

/// The orange of the logo: selection, active toggles, keyframes.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x6A, 0x1F);
/// The accent darkened enough to carry light text on top of it.
pub(crate) const ACCENT_FILL: Color32 = Color32::from_rgb(0xA8, 0x45, 0x14);
/// Translucent accent for box selections.
pub(crate) const ACCENT_TRANSLUCENT: Color32 = Color32::from_rgba_premultiplied(40, 17, 5, 40);
/// Errors: media offline, audio clipping.
pub(crate) const ERROR: Color32 = Color32::from_rgb(230, 70, 70);
pub(crate) const PLAYHEAD: Color32 = Color32::from_rgb(220, 50, 50);

pub(crate) fn apply(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.visuals.selection.bg_fill = ACCENT_FILL;
            style.visuals.selection.stroke.color = Color32::WHITE;
            style.visuals.hyperlink_color = ACCENT;
        });
    }
}
