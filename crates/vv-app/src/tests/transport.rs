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
