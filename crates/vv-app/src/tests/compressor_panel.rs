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

#[test]
fn saving_a_preset_under_an_existing_name_replaces_it() {
    let mut presets = Vec::new();
    let mut params = MultibandCompressor::DEFAULT;
    save_preset(&mut presets, "A", &params);
    params.bands[0].ratio = 8.0;
    save_preset(&mut presets, "B", &params);
    save_preset(&mut presets, "A", &params);
    let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["A", "B"]);
    assert_eq!(presets[0].params, params);
}

#[test]
fn the_builtin_presets_are_distinct_valid_and_translated() {
    let presets = builtin_presets();
    let within = |v: f32, (min, max): (f32, f32)| (min..=max).contains(&v);
    for (i, (key, preset)) in presets.iter().enumerate() {
        assert_ne!(t!(*key), *key, "{key} has no text");
        let [low, high] = preset.crossovers_hz;
        assert!(low * MIN_CROSSOVER_RATIO <= high, "{key}");
        for band in &preset.bands {
            assert!(
                within(band.threshold_db, CompressorBand::THRESHOLD_RANGE_DB),
                "{key}"
            );
            assert!(within(band.ratio, CompressorBand::RATIO_RANGE), "{key}");
            assert!(
                within(band.attack_ms, CompressorBand::ATTACK_RANGE_MS),
                "{key}"
            );
            assert!(
                within(band.release_ms, CompressorBand::RELEASE_RANGE_MS),
                "{key}"
            );
            assert!(
                within(band.makeup_db, CompressorBand::MAKEUP_RANGE_DB),
                "{key}"
            );
        }
        assert!(
            presets[..i].iter().all(|(_, other)| other != preset),
            "{key}"
        );
    }
    assert_eq!(presets[0].1, MultibandCompressor::DEFAULT);
}

/// Pink noise with peaks at -6 dBFS, about the level of a mix.
fn pink_noise(frames: usize) -> Vec<f32> {
    let mut seed = 7u32;
    let mut b = [0f32; 7];
    let mut pink: Vec<f32> = (0..frames)
        .map(|_| {
            let white = white_noise(&mut seed);
            // Paul Kellet's filter.
            b[0] = 0.99886 * b[0] + white * 0.0555179;
            b[1] = 0.99332 * b[1] + white * 0.0750759;
            b[2] = 0.96900 * b[2] + white * 0.153852;
            b[3] = 0.86650 * b[3] + white * 0.3104856;
            b[4] = 0.55000 * b[4] + white * 0.5329522;
            b[5] = -0.7616 * b[5] - white * 0.0168980;
            let out = b.iter().sum::<f32>() + white * 0.5362;
            b[6] = white * 0.115926;
            out
        })
        .collect();
    let peak = pink.iter().fold(0f32, |p, s| p.max(s.abs()));
    let gain = 10f32.powf(-6.0 / 20.0) / peak;
    pink.iter_mut().for_each(|s| *s *= gain);
    pink
}

fn white_noise(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (*seed >> 8) as f32 / 8388608.0 - 1.0
}

fn rms_db(samples: &[f32]) -> f32 {
    let power = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    10.0 * power.log10()
}

/// Mean reduction of each band past the first 0.25 s, and the output.
fn run_preset(params: &MultibandCompressor, input: &[f32]) -> ([f32; 3], Vec<f32>) {
    let mut processor = vv_audio::dynamics::MultibandProcessor::new(params, 48_000, 1);
    let mut out = input.to_vec();
    let mut sum = [0f32; 3];
    let mut counted = 0;
    for (i, chunk) in out.chunks_mut(512).enumerate() {
        let activity = processor.process(chunk);
        if i * 512 >= 12_000 {
            for (s, r) in sum.iter_mut().zip(activity.reduction_db) {
                *s += r;
            }
            counted += 1;
        }
    }
    (sum.map(|s| s / counted as f32), out)
}

#[test]
fn every_preset_audibly_does_its_job_on_a_mix_level_signal() {
    let input = pink_noise(48_000 * 2);
    let presets = builtin_presets();
    let find = |key: &str| presets.iter().find(|(k, _)| *k == key).unwrap().1.clone();
    let in_db = rms_db(&input);

    for key in [
        "mixer.preset_default",
        "mixer.preset_glue",
        "mixer.preset_master",
    ] {
        let (reduction, out) = run_preset(&find(key), &input);
        let most = reduction.iter().copied().fold(0.0, f32::max);
        assert!((2.0..8.0).contains(&most), "{key}: {reduction:?}");
        if key != "mixer.preset_default" {
            assert!((rms_db(&out) - in_db).abs() < 1.5, "{key}: level-matched");
        }
    }

    let (reduction, _) = run_preset(&find("mixer.preset_low_end"), &input);
    assert!(
        reduction[0] > 3.0 && reduction[1] == 0.0 && reduction[2] == 0.0,
        "{reduction:?}"
    );

    let (_, out) = run_preset(&find("mixer.preset_broadcast"), &input);
    let peak_db = 20.0 * out.iter().fold(0f32, |p, s| p.max(s.abs())).log10();
    assert!(rms_db(&out) - in_db > 2.0, "louder than what goes in");
    assert!(peak_db < -1.0, "without clipping: {peak_db}");

    // Steady highs pass; a sibilant 10 dB over them is caught.
    let deesser = find("mixer.preset_deesser");
    let (steady, _) = run_preset(&deesser, &input);
    assert!(steady[2] < 1.0, "{steady:?}");
    let mut hiss = input.clone();
    let mut seed = 99u32;
    for s in &mut hiss {
        *s += white_noise(&mut seed) * 0.5;
    }
    let (hissing, _) = run_preset(&deesser, &hiss);
    assert!(hissing[2] > 5.0 && hissing[0] == 0.0, "{hissing:?}");
}
