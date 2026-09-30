use super::*;

const RATE: u32 = 48_000;

fn sine(freq: f32, amplitude: f32, seconds: f32) -> Vec<f32> {
    let n = (RATE as f32 * seconds) as usize;
    (0..n)
        .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
        .collect()
}

/// Peak of the last quarter, once filters and envelopes have settled.
fn settled_peak(samples: &[f32]) -> f32 {
    samples[samples.len() * 3 / 4..]
        .iter()
        .fold(0.0, |p, s| p.max(s.abs()))
}

fn run(params: &MultibandCompressor, mut samples: Vec<f32>) -> Vec<f32> {
    MultibandProcessor::new(params, RATE, 1).process(&mut samples);
    samples
}

fn off() -> MultibandCompressor {
    let mut params = MultibandCompressor::DEFAULT;
    for band in &mut params.bands {
        band.ratio = 1.0;
    }
    params
}

#[test]
fn with_nothing_to_compress_the_bands_sum_back_to_the_input_level() {
    for freq in [60.0, 200.0, 700.0, 2000.0, 8000.0] {
        let peak = settled_peak(&run(&off(), sine(freq, 0.5, 0.5)));
        assert!((peak - 0.5).abs() < 0.01, "{freq} Hz: {peak}");
    }
}

#[test]
fn a_loud_band_is_brought_down_by_its_ratio() {
    let mut params = off();
    params.bands[1] = CompressorBand {
        threshold_db: -20.0,
        ratio: 4.0,
        ..CompressorBand::DEFAULT
    };
    // 0 dBFS, 20 dB over the threshold: 15 dB of reduction, a little less
    // as the envelope of a sine sits under its peak.
    let peak = settled_peak(&run(&params, sine(700.0, 1.0, 1.0)));
    let peak_db = 20.0 * peak.log10();
    assert!((-15.0..-12.5).contains(&peak_db), "{peak_db} dB");

    let mut samples = sine(700.0, 1.0, 1.0);
    let reported = MultibandProcessor::new(&params, RATE, 1)
        .process(&mut samples)
        .reduction_db;
    assert!((12.5..15.5).contains(&reported[1]), "{reported:?}");
    assert!(reported[0] < 0.5 && reported[2] < 0.5, "{reported:?}");
}

#[test]
fn a_band_leaves_the_others_alone_and_makeup_raises_it() {
    let mut params = off();
    params.bands[0] = CompressorBand {
        threshold_db: -60.0,
        ratio: 20.0,
        ..CompressorBand::DEFAULT
    };
    params.bands[2].makeup_db = 6.0;
    let high = settled_peak(&run(&params, sine(8000.0, 0.25, 0.5)));
    assert!((high - 0.5).abs() < 0.02, "{high}");
}

#[test]
fn reset_forgets_the_envelope() {
    let mut params = off();
    params.bands[1].threshold_db = -40.0;
    params.bands[1].ratio = 10.0;
    let mut processor = MultibandProcessor::new(&params, RATE, 1);
    processor.process(&mut sine(700.0, 1.0, 0.5));
    processor.reset();
    let mut quiet = vec![0.0f32; 16];
    quiet[0] = 0.001;
    processor.process(&mut quiet);
    assert!(quiet.iter().all(|s| s.is_finite() && s.abs() < 0.01));
    assert_eq!(processor.envelope.iter().filter(|e| **e > 0.1).count(), 0);
}

#[test]
fn the_drawn_response_is_flat_at_unity_and_follows_each_band() {
    let params = MultibandCompressor::DEFAULT;
    let response = CrossoverResponse::new(&params, RATE);
    for freq in [30.0, 200.0, 1000.0, 2000.0, 12_000.0] {
        assert!(response.gain_db([0.0; 3], freq).abs() < 0.01, "{freq} Hz");
    }
    let mid_down = [0.0, -12.0, 0.0];
    assert!((response.gain_db(mid_down, 630.0) + 12.0).abs() < 1.0);
    assert!(response.gain_db(mid_down, 30.0).abs() < 0.5);
    assert!(response.gain_db(mid_down, 15_000.0).abs() < 0.5);
}
