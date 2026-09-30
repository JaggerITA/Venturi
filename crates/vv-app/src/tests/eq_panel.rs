use super::*;

#[test]
fn the_builtin_presets_are_distinct_valid_and_translated() {
    let presets = builtin_presets();
    let within = |v: f32, (min, max): (f32, f32)| (min..=max).contains(&v);
    for (i, (key, preset)) in presets.iter().enumerate() {
        assert_ne!(t!(*key), *key, "{key} has no text");
        for band in &preset.bands {
            assert!(within(band.freq_hz, EqBand::FREQ_RANGE_HZ), "{key}");
            assert!(within(band.gain_db, EqBand::GAIN_RANGE_DB), "{key}");
            assert!(within(band.q, EqBand::Q_RANGE), "{key}");
            assert!(band.shape.has_gain() || band.gain_db == 0.0, "{key}");
        }
        assert!(
            presets[..i].iter().all(|(_, other)| other != preset),
            "{key}"
        );
    }
    assert_eq!(presets[0].1, Equalizer::DEFAULT);
}

#[test]
fn every_shape_has_a_name() {
    for shape in EqShape::ALL {
        let name = shape_name(shape);
        assert!(!name.starts_with("mixer."), "{name}");
    }
}

#[test]
fn the_wheel_scales_q_by_steps_and_keeps_it_in_range() {
    let q = wheel_q(1.0, 1.0);
    assert_eq!(q, 1.15);
    assert_eq!(wheel_q(q, -1.0), 1.0);
    assert_eq!(wheel_q(17.0, 5.0), EqBand::Q_RANGE.1);
    assert_eq!(wheel_q(0.12, -5.0), EqBand::Q_RANGE.0);
}

#[test]
fn a_cut_sits_on_the_zero_line_whatever_its_gain() {
    let mut band = EqBand::new(EqShape::HighPass, 100.0, true);
    band.gain_db = 6.0;
    assert_eq!(point_db(&band), 0.0);
    band.shape = EqShape::Peak;
    assert_eq!(point_db(&band), 6.0);
}
