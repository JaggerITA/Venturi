use super::*;

#[test]
fn the_frequency_axis_is_logarithmic_from_20_hz_to_20_khz() {
    assert_eq!(freq_fraction(20.0), 0.0);
    assert!((freq_fraction(20_000.0) - 1.0).abs() < 1e-6);
    assert!(
        (freq_fraction(632.46) - 0.5).abs() < 1e-3,
        "geometric middle"
    );
    for freq in [35.0, 200.0, 2000.0, 15_000.0] {
        assert!((fraction_freq(freq_fraction(freq)) - freq).abs() / freq < 1e-4);
    }
}

#[test]
fn log_knobs_spread_their_range_evenly_by_ratio() {
    let range = (0.1, 200.0);
    assert_eq!(knob_fraction(0.1, range, true), 0.0);
    assert!((knob_fraction(200.0, range, true) - 1.0).abs() < 1e-6);
    let middle = knob_value(0.5, range, true);
    assert!((middle - (0.1f32 * 200.0).sqrt()).abs() < 1e-3);
    assert!((knob_value(knob_fraction(10.0, range, true), range, true) - 10.0).abs() < 1e-3);
    assert_eq!(knob_fraction(6.0, (-12.0, 24.0), false), 0.5);
    assert_eq!(knob_fraction(99.0, (-12.0, 24.0), false), 1.0, "clamped");
}

#[test]
fn frequencies_read_short() {
    assert_eq!(format_freq(200.0), "200");
    assert_eq!(format_freq(2000.0), "2k");
    assert_eq!(format_freq(2500.0), "2.5k");
}
