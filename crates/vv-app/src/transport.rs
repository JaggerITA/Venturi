//! Barra di riproduzione sotto al viewer: testina, marker in/out e
//! pulsante play/pausa. Vale sia per l'anteprima di un media del media pool
//! (in/out = porzione da portare sulla timeline) sia per la timeline
//! (in/out = porzione da esportare).

use vv_core::FrameIdx;

/// Marker in/out: `None` = estremo della barra, così seguono la durata
/// quando il contenuto cambia.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkRange {
    pub mark_in: Option<FrameIdx>,
    /// Esclusivo: "o" sul frame `p` lo include.
    pub mark_out: Option<FrameIdx>,
}

impl MarkRange {
    /// `(in, out)` effettivi dentro `0..=total`, con `in <= out`.
    pub fn resolve(&self, total: FrameIdx) -> (FrameIdx, FrameIdx) {
        let total = total.max(0);
        let out = self.mark_out.unwrap_or(total).clamp(0, total);
        let mark_in = self.mark_in.unwrap_or(0).clamp(0, out);
        (mark_in, out)
    }

    pub fn set_in(&mut self, frame: FrameIdx, total: FrameIdx) {
        let frame = frame.clamp(0, total.max(0));
        self.mark_in = Some(frame);
        if self.mark_out.is_some_and(|out| out <= frame) {
            self.mark_out = None;
        }
    }

    pub fn set_out(&mut self, frame: FrameIdx, total: FrameIdx) {
        let out = (frame + 1).clamp(0, total.max(0));
        self.mark_out = Some(out);
        if self.mark_in.is_some_and(|mark_in| mark_in >= out) {
            self.mark_in = None;
        }
    }

    pub fn is_full(&self, total: FrameIdx) -> bool {
        self.resolve(total) == (0, total.max(0))
    }
}

#[derive(Default)]
pub struct TransportResponse {
    pub seek: Option<FrameIdx>,
    pub toggle_play: bool,
}

const BAR_HEIGHT: f32 = 30.0;
const TRACK_Y: f32 = 12.0;
const SIDE_PADDING: f32 = 10.0;
const PLAY_BUTTON_SIZE: f32 = 26.0;

/// `total` frame di contenuto; la testina può stare su `0..total`.
pub fn show_transport(
    ui: &mut egui::Ui,
    total: FrameIdx,
    playhead: FrameIdx,
    marks: (FrameIdx, FrameIdx),
    playing: bool,
) -> TransportResponse {
    let mut response = TransportResponse::default();
    let (rect, bar_resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), BAR_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let track = egui::Rect::from_min_max(
        egui::pos2(rect.left() + SIDE_PADDING, rect.top() + TRACK_Y - 2.0),
        egui::pos2(rect.right() - SIDE_PADDING, rect.top() + TRACK_Y + 2.0),
    );
    let span = total.max(1) as f32;
    let x_of = |frame: FrameIdx| track.left() + track.width() * (frame as f32 / span).clamp(0.0, 1.0);

    if total > 0
        && let Some(pos) = bar_resp.interact_pointer_pos()
    {
        let frac = ((pos.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
        let frame = ((frac * span).round() as FrameIdx).min(total - 1);
        if frame != playhead {
            response.seek = Some(frame);
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(track, 2.0, egui::Color32::from_gray(60));
    let (mark_in, mark_out) = marks;
    let region = egui::Rect::from_min_max(
        egui::pos2(x_of(mark_in), track.top()),
        egui::pos2(x_of(mark_out), track.bottom()),
    );
    painter.rect_filled(region, 2.0, egui::Color32::from_gray(150));

    let marker_color = egui::Color32::from_gray(185);
    for (frame, is_in) in [(mark_in, true), (mark_out, false)] {
        let x = x_of(frame);
        painter.vline(
            x,
            track.top()..=track.bottom() + 6.0,
            egui::Stroke::new(1.0, marker_color),
        );
        // Bandierina rivolta verso l'interno della regione.
        let flag_w = if is_in { 7.0 } else { -7.0 };
        let top = track.bottom() + 6.0;
        let mut points = vec![
            egui::pos2(x, top),
            egui::pos2(x + flag_w, top + 3.0),
            egui::pos2(x + flag_w, top + 10.0),
            egui::pos2(x, top + 10.0),
        ];
        // Il tessellatore di egui vuole i vertici in senso orario.
        if !is_in {
            points.reverse();
        }
        painter.add(egui::Shape::convex_polygon(
            points,
            marker_color,
            egui::Stroke::NONE,
        ));
    }

    let playhead_color = egui::Color32::from_rgb(220, 50, 50);
    let px = x_of(playhead);
    painter.vline(
        px,
        rect.top() + 2.0..=track.bottom() + 4.0,
        egui::Stroke::new(2.0, playhead_color),
    );
    painter.rect_filled(
        egui::Rect::from_center_size(egui::pos2(px, rect.top() + 6.0), egui::vec2(10.0, 10.0)),
        2.0,
        playhead_color,
    );

    ui.vertical_centered(|ui| {
        let (button_rect, button_resp) = ui.allocate_exact_size(
            egui::vec2(PLAY_BUTTON_SIZE, PLAY_BUTTON_SIZE),
            egui::Sense::click(),
        );
        let visuals = ui.style().interact(&button_resp);
        ui.painter()
            .rect_filled(button_rect, 4.0, visuals.weak_bg_fill);
        let color = visuals.fg_stroke.color;
        let c = button_rect.center();
        // Icone disegnate: i glifi ▶/⏸ non sono garantiti nei font di egui.
        if playing {
            for dx in [-4.0, 4.0] {
                ui.painter().rect_filled(
                    egui::Rect::from_center_size(c + egui::vec2(dx, 0.0), egui::vec2(4.0, 12.0)),
                    1.0,
                    color,
                );
            }
        } else {
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    c + egui::vec2(-4.0, -7.0),
                    c + egui::vec2(7.0, 0.0),
                    c + egui::vec2(-4.0, 7.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        if button_resp
            .on_hover_text("Play/pausa (Spazio) · I/O: punto di inizio/fine")
            .clicked()
        {
            response.toggle_play = true;
        }
    });

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_marks_span_the_whole_content_and_follow_its_length() {
        let marks = MarkRange::default();
        assert_eq!(marks.resolve(100), (0, 100));
        assert_eq!(marks.resolve(40), (0, 40));
        assert!(marks.is_full(40));
    }

    #[test]
    fn out_includes_the_marked_frame() {
        let mut marks = MarkRange::default();
        marks.set_in(10, 100);
        marks.set_out(29, 100);
        assert_eq!(marks.resolve(100), (10, 30));
        assert!(!marks.is_full(100));
    }

    #[test]
    fn marking_past_the_other_marker_resets_it_to_the_edge() {
        let mut marks = MarkRange::default();
        marks.set_out(20, 100);
        marks.set_in(50, 100);
        assert_eq!(marks.resolve(100), (50, 100));

        marks.set_out(30, 100);
        assert_eq!(marks.resolve(100), (0, 31));
    }

    #[test]
    fn marks_are_clamped_when_the_content_shrinks() {
        let mut marks = MarkRange::default();
        marks.set_in(80, 100);
        assert_eq!(marks.resolve(50), (50, 50));
    }
}
