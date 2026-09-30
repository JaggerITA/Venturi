use super::*;

const RATE: f32 = 48_000.0;

#[test]
fn a_full_scale_sine_reads_0_db_in_its_bin_and_little_elsewhere() {
    let bin = 100;
    let freq = bin as f32 * RATE / SPECTRUM_SIZE as f32;
    let samples: Vec<f32> = (0..SPECTRUM_SIZE)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin())
        .collect();
    let db = magnitudes_db(&samples);
    assert_eq!(db.len(), SPECTRUM_SIZE / 2 + 1);
    assert!(db[bin].abs() < 0.1, "{}", db[bin]);
    assert!(db[bin + 10] < -60.0 && db[bin - 10] < -60.0);
}

#[test]
fn the_tap_reads_back_the_last_frames_in_order_mixed_to_mono() {
    let tap = SpectrumTap::default();
    let block: Vec<f32> = (0..3000).flat_map(|i| [i as f32, i as f32 + 2.0]).collect();
    for chunk in [&block[..2000], &block[2000..]] {
        tap.record_pre(chunk, 2);
        let doubled: Vec<f32> = chunk.iter().map(|s| s * 2.0).collect();
        tap.record_post(&doubled, 2);
    }
    tap.record_pre(&block, 2);
    tap.record_post(&block, 2);
    assert_eq!(tap.written(), 6000);

    let (pre, post) = tap.read();
    assert_eq!(pre.len(), SPECTRUM_SIZE);
    let first = 6000 - SPECTRUM_SIZE;
    let expected = |i: usize| (i % 3000) as f32 + 1.0;
    assert_eq!(pre[0], expected(first));
    assert_eq!(pre[SPECTRUM_SIZE - 1], expected(5999));
    assert_eq!(post[SPECTRUM_SIZE - 1], expected(5999));
}
