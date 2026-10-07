use super::*;

fn probabilities(samples: &[f32], piece: usize) -> Vec<f32> {
    let mut vad = Vad::new().unwrap();
    for chunk in samples.chunks(piece) {
        vad.push(chunk).unwrap();
    }
    vad.finish().unwrap()
}

/// A short click every quarter of a second, like typing.
fn clicks(secs: f32) -> Vec<f32> {
    let len = (secs * SAMPLE_RATE as f32) as usize;
    (0..len)
        .map(|i| match i % 4000 {
            0..40 if i % 2 == 0 => 0.6,
            0..40 => -0.6,
            _ => 0.0,
        })
        .collect()
}

#[test]
fn silence_is_not_speech() {
    let probabilities = probabilities(&vec![0.0; SAMPLE_RATE as usize], 4096);
    assert!(probabilities.iter().all(|&p| p < 0.1), "{probabilities:?}");
}

#[test]
fn keyboard_clicks_are_not_speech() {
    let probabilities = probabilities(&clicks(3.0), 4096);
    assert!(probabilities.iter().all(|&p| p < 0.3), "{probabilities:?}");
}

#[test]
fn one_probability_per_chunk_the_last_padded() {
    assert_eq!(probabilities(&vec![0.0; CHUNK * 3 + 1], 4096).len(), 4);
    assert_eq!(probabilities(&vec![0.0; CHUNK * 3], 4096).len(), 3);
    assert!(probabilities(&[], 4096).is_empty());
}

#[test]
fn the_result_does_not_depend_on_how_the_samples_are_split() {
    let samples = clicks(1.0);
    assert_eq!(probabilities(&samples, 100), probabilities(&samples, 7000));
}
