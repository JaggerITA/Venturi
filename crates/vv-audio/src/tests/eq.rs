use super::*;
use vv_core::EqShape;

const RATE: u32 = 48_000;

fn sine(freq: f32, seconds: f32) -> Vec<f32> {
    let n = (RATE as f32 * seconds) as usize;
    (0..n)
        .map(|i| 0.25 * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
        .collect()
}

/// Gain in dB of the last half, once the filters have settled.
fn measured_gain_db(params: &Equalizer, freq: f32) -> f32 {
    let mut samples = sine(freq, 0.5);
    EqProcessor::new(params, RATE, 1).process(&mut samples);
    let tail = &samples[samples.len() / 2..];
    let peak = tail.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    20.0 * (peak / 0.25).log10()
}

fn flat() -> Equalizer {
    let mut params = Equalizer::DEFAULT;
    params.bands.iter_mut().for_each(|b| b.enabled = false);
    params
}

#[test]
fn the_default_leaves_the_audible_range_untouched() {
    let params = Equalizer::DEFAULT;
    let mut samples = sine(1000.0, 0.2);
    let original = samples.clone();
    EqProcessor::new(&params, RATE, 1).process(&mut samples);
    assert_eq!(samples, original);
}

#[test]
fn a_peak_raises_its_frequency_by_its_gain_and_leaves_the_far_ones() {
    let mut params = flat();
    params.bands[2] = EqBand {
        enabled: true,
        shape: EqShape::Peak,
        freq_hz: 1000.0,
        gain_db: 9.0,
        q: 1.0,
    };
    assert!((measured_gain_db(&params, 1000.0) - 9.0).abs() < 0.2);
    assert!(measured_gain_db(&params, 60.0).abs() < 0.3);
    assert!(measured_gain_db(&params, 15_000.0).abs() < 0.3);
}

#[test]
fn shelves_move_their_side_and_cuts_remove_it() {
    let mut params = flat();
    params.bands[1] = EqBand {
        enabled: true,
        gain_db: -6.0,
        ..EqBand::new(EqShape::LowShelf, 200.0, true)
    };
    params.bands[5] = EqBand::new(EqShape::LowPass, 2000.0, true);
    assert!((measured_gain_db(&params, 40.0) + 6.0).abs() < 0.3);
    assert!(measured_gain_db(&params, 800.0).abs() < 1.0);
    assert!(measured_gain_db(&params, 12_000.0) < -24.0);
}

#[test]
fn the_drawn_response_matches_what_is_heard() {
    let mut params = Equalizer::DEFAULT;
    params.bands[0].enabled = true;
    params.bands[2].gain_db = 6.0;
    params.bands[4].gain_db = -4.0;
    let response = EqResponse::new(&params, RATE);
    for freq in [50.0, 300.0, 500.0, 3000.0, 10_000.0] {
        let drawn = response.gain_db(freq);
        let heard = measured_gain_db(&params, freq);
        assert!((drawn - heard).abs() < 0.3, "{freq} Hz: {drawn} vs {heard}");
    }
    assert_eq!(response.band_gain_db(3, 2500.0), 0.0, "flat, so off");
}
