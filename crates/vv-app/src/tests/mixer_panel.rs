use super::*;

#[test]
fn fader_curve_puts_unity_at_its_mark_and_spans_the_whole_gain_range() {
    assert_eq!(fader_position(0.0), 0.7);
    assert_eq!(fader_position(vv_core::GAIN_DB_MIN), 0.0);
    assert_eq!(fader_position(vv_core::GAIN_DB_MAX), 1.0);
    assert_eq!(fader_position(-1000.0), 0.0, "clamped");
}

#[test]
fn fader_db_inverts_fader_position() {
    for db in [-100.0, -73.5, -30.0, -7.2, 0.0, 4.5, 30.0] {
        assert!((fader_db(fader_position(db)) - db).abs() < 1e-3, "{db}");
    }
}

#[test]
fn pan_reads_as_centre_or_side_percent() {
    assert_eq!(format_pan(0.0), "C");
    assert_eq!(format_pan(0.001), "C");
    assert_eq!(format_pan(-0.3), "L30");
    assert_eq!(format_pan(1.0), "R100");
}
