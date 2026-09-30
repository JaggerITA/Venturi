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

#[test]
fn a_spectrum_peak_lands_on_the_point_of_its_frequency() {
    let rate = 48_000;
    let bins = vv_audio::spectrum::SPECTRUM_SIZE / 2 + 1;
    let bin_hz = rate as f32 / vv_audio::spectrum::SPECTRUM_SIZE as f32;
    let mut db = vec![-90.0f32; bins];
    let peak_bin = (1000.0 / bin_hz).round() as usize;
    db[peak_bin] = -10.0;
    let points = spectrum_at_points(&db, rate);
    assert_eq!(points.len(), CURVE_POINTS + 1);
    let loudest = (0..points.len())
        .max_by(|a, b| points[*a].total_cmp(&points[*b]))
        .unwrap();
    assert!(
        (curve_freq(loudest) / 1000.0 - 1.0).abs() < 0.05,
        "{}",
        curve_freq(loudest)
    );
    assert_eq!(points[loudest], -10.0);
    assert!(
        points.iter().all(|p| p.is_finite()),
        "the low end interpolates"
    );
}

#[test]
fn the_spectrum_falls_away_once_the_audio_stops() {
    let tap = vv_audio::spectrum::SpectrumTap::default();
    let loud: Vec<f32> = (0..vv_audio::spectrum::SPECTRUM_SIZE)
        .map(|i| (i as f32 * 0.3).sin())
        .collect();
    tap.record_pre(&loud, 1);
    tap.record_post(&loud, 1);
    let mut view = SpectrumView::default();
    assert!(view.update(Some(&tap)));
    let top = view.pre.iter().copied().fold(f32::MIN, f32::max);
    assert!(top > -12.0, "{top}");

    view.update(Some(&tap));
    let fallen = view.pre.iter().copied().fold(f32::MIN, f32::max);
    assert!(
        (top - fallen - SPECTRUM_FALL_DB).abs() < 1e-3,
        "no new audio"
    );
    for _ in 0..200 {
        view.update(None);
    }
    assert!(!view.update(None));
}

#[test]
fn smoothing_spreads_a_spike_by_its_power_and_keeps_a_flat_run() {
    let mut points = vec![-60.0f32; 20];
    points[10] = -10.0;
    let smoothed = smooth(&points);
    assert_eq!(smoothed.len(), 20);
    assert!((smoothed[10] + 16.99).abs() < 0.01, "{}", smoothed[10]);
    assert!((smoothed[0] + 60.0).abs() < 1e-3);
}
