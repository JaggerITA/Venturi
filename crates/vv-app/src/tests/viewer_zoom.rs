use super::*;

fn area() -> Rect {
    Rect::from_min_size(Pos2::ZERO, egui::vec2(960.0, 540.0))
}

const FRAME: Vec2 = egui::vec2(1920.0, 1080.0);

#[test]
fn fit_fills_the_viewer() {
    let zoom = ViewerZoom::default();
    assert_eq!(zoom.scale(area(), FRAME, 1.0), 0.5);
    assert_eq!(zoom.scale(area(), FRAME, 2.0), 1.0);
    assert_eq!(zoom.frame_rect(area(), FRAME, 2.0), area());
}

#[test]
fn actual_size_maps_frame_pixels_to_physical_pixels() {
    let mut zoom = ViewerZoom::default();
    zoom.set_scale(1.0, area(), FRAME, 2.0);
    assert_eq!(zoom.frame_rect(area(), FRAME, 2.0).size(), egui::vec2(960.0, 540.0));
    zoom.set_scale(1.0, area(), FRAME, 1.0);
    assert_eq!(zoom.frame_rect(area(), FRAME, 1.0).size(), FRAME);
}

#[test]
fn zooming_keeps_the_point_under_the_pointer_still() {
    let mut zoom = ViewerZoom::default();
    let pointer = egui::pos2(700.0, 100.0);
    let before = zoom.frame_rect(area(), FRAME, 1.0);
    let uv = (pointer - before.min) / before.size();
    zoom.zoom_around(pointer, 3.0, area(), FRAME, 1.0);
    let after = zoom.frame_rect(area(), FRAME, 1.0);
    assert_eq!(zoom.scale(area(), FRAME, 1.0), 1.5);
    let moved = after.min + uv * after.size();
    assert!((moved - pointer).length() < 1e-3, "{moved:?}");
}

#[test]
fn pan_keeps_part_of_the_frame_visible() {
    let mut zoom = ViewerZoom::default();
    zoom.set_scale(0.5, area(), FRAME, 1.0);
    zoom.pan = egui::vec2(10_000.0, -10_000.0);
    zoom.clamp_pan(area(), FRAME, 1.0);
    let rect = zoom.frame_rect(area(), FRAME, 1.0);
    assert_eq!(rect.left(), area().right() - MIN_VISIBLE);
    assert_eq!(rect.bottom(), area().top() + MIN_VISIBLE);
}

#[test]
fn percent_labels() {
    assert_eq!(percent_label(3.52), "352%");
    assert_eq!(percent_label(0.0625), "6.25%");
    assert_eq!(percent_label(0.125), "12.5%");
    assert_eq!(percent_label(1.0), "100%");
}
