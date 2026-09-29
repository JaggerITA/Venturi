use super::*;

fn marker(start: FrameIdx, duration: FrameIdx) -> Marker {
    Marker {
        id: MarkerId(0),
        start,
        duration,
        note: String::new(),
        color: Marker::default_color(),
    }
}

fn preview(
    mode: DragMode,
    original: Marker,
    grab: FrameIdx,
    pointer: FrameIdx,
) -> (FrameIdx, FrameIdx) {
    let drag = MarkerDrag {
        mode,
        preview: original.clone(),
        original,
        grab_frame: grab,
    };
    let p = drag_preview(&drag, pointer, |frame, _| frame);
    (p.start, p.duration)
}

#[test]
fn a_moved_marker_stops_at_frame_zero() {
    assert_eq!(preview(DragMode::Move, marker(10, 5), 12, 0), (0, 5));
}

#[test]
fn resizing_past_the_other_end_collapses_to_a_single_frame() {
    assert_eq!(preview(DragMode::Start, marker(10, 5), 10, 40), (15, 0));
    assert_eq!(preview(DragMode::End, marker(10, 5), 15, 2), (10, 0));
}

#[test]
fn adding_at_the_playhead_skips_a_frame_that_already_has_a_marker() {
    let mut project = Project::default();
    let timeline_id = project.timelines.insert(vv_core::Timeline {
        name: "T".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: Vec::new(),
        markers: Vec::new(),
        master: Default::default(),
    });
    let mut history = vv_core::History::default();
    add_marker_at_playhead(&mut project, &mut history, timeline_id, 12);
    add_marker_at_playhead(&mut project, &mut history, timeline_id, 12);
    add_marker_at_playhead(&mut project, &mut history, timeline_id, 30);
    let markers = &project.timelines[timeline_id].markers;
    assert_eq!(
        markers.iter().map(|m| m.start).collect::<Vec<_>>(),
        [12, 30]
    );
    assert_ne!(markers[0].id, markers[1].id);
    assert_eq!(history.position(), 2);
}
