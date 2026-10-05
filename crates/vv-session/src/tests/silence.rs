use super::*;
use std::ops::ControlFlow;
use vv_core::{ClipId, ClipSource, Id, MediaId, Rational};

const W: f64 = WINDOW_SECS;

/// `(probability, windows)` per entry.
fn speech(runs: &[(f32, usize)]) -> SpeechProbabilities {
    SpeechProbabilities(
        runs.iter()
            .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
            .collect(),
    )
}

fn params(min_silence_secs: f64, head_secs: f64, tail_secs: f64) -> SilenceParams {
    SilenceParams {
        threshold: 0.5,
        min_silence_secs,
        head_secs,
        tail_secs,
    }
}

fn assert_spans(actual: &[(f64, f64)], expected: &[(f64, f64)], tolerance: f64) {
    assert_eq!(actual.len(), expected.len(), "{actual:?} != {expected:?}");
    for (a, e) in actual.iter().zip(expected) {
        assert!(
            (a.0 - e.0).abs() <= tolerance && (a.1 - e.1).abs() <= tolerance,
            "{actual:?} != {expected:?}"
        );
    }
}

#[test]
fn spans_are_the_gaps_between_speech() {
    let speech = speech(&[(0.9, 30), (0.0, 20), (0.9, 30)]);
    assert_spans(
        &silent_spans(&speech, &params(0.0, 0.0, 0.0)),
        &[(30.0 * W, 50.0 * W)],
        1e-9,
    );
}

#[test]
fn speech_fading_out_lasts_until_well_under_the_threshold() {
    let speech = speech(&[(0.9, 30), (0.4, 10), (0.0, 20), (0.9, 30)]);
    assert_spans(
        &silent_spans(&speech, &params(0.0, 0.0, 0.0)),
        &[(40.0 * W, 60.0 * W)],
        1e-9,
    );
}

#[test]
fn a_detection_too_short_for_speech_is_silence() {
    let speech = speech(&[(0.0, 30), (0.9, 2), (0.0, 30), (0.9, 30)]);
    assert_spans(
        &silent_spans(&speech, &params(0.0, 0.0, 0.0)),
        &[(0.0, 62.0 * W)],
        1e-9,
    );
}

#[test]
fn pauses_shorter_than_the_minimum_are_kept() {
    let speech = speech(&[(0.9, 30), (0.0, 5), (0.9, 30), (0.0, 20), (0.9, 30)]);
    assert_spans(
        &silent_spans(&speech, &params(0.3, 0.0, 0.0)),
        &[(65.0 * W, 85.0 * W)],
        1e-9,
    );
}

#[test]
fn head_and_tail_shrink_the_span() {
    let speech = speech(&[(0.9, 30), (0.0, 30), (0.9, 30)]);
    assert_spans(
        &silent_spans(&speech, &params(0.0, 2.0 * W, W)),
        &[(31.0 * W, 58.0 * W)],
        1e-9,
    );
}

#[test]
fn the_silences_at_the_ends_of_the_media_keep_no_margin_there() {
    let speech = speech(&[(0.0, 20), (0.9, 30), (0.0, 20)]);
    assert_spans(
        &silent_spans(&speech, &params(0.0, 2.0 * W, 2.0 * W)),
        &[(0.0, 18.0 * W), (52.0 * W, 70.0 * W)],
        1e-9,
    );
}

#[test]
fn a_span_eaten_by_head_and_tail_disappears() {
    let speech = speech(&[(0.9, 30), (0.0, 5), (0.9, 30)]);
    assert!(silent_spans(&speech, &params(0.0, 0.1, 0.1)).is_empty());
}

fn clip(timeline_start: FrameIdx, source_offset: FrameIdx, len: FrameIdx, speed: Rational) -> Clip {
    Clip::new(
        ClipId(1),
        ClipSource::Media(MediaId::from_raw(0)),
        source_offset,
        timeline_start,
        len,
        Rational::one(),
        speed,
    )
}

#[test]
fn cut_ranges_follow_the_clip_position_and_trim() {
    // Media second 2 is frame 50 of the source, trimmed from frame 25.
    let clip = clip(100, 25, 100, Rational::one());
    assert_eq!(clip_cut_ranges(&clip, 25.0, &[(2.0, 3.0)]), [(125, 150)]);
}

#[test]
fn cut_ranges_round_inwards_and_stay_in_the_clip() {
    let clip = clip(0, 0, 50, Rational::one());
    assert_eq!(
        clip_cut_ranges(&clip, 25.0, &[(0.01, 0.99), (1.5, 9.0), (5.0, 6.0)]),
        [(1, 24), (38, 50)]
    );
}

#[test]
fn cut_ranges_follow_the_clip_speed() {
    let clip = clip(0, 0, 100, Rational::new(2, 1));
    assert_eq!(clip_cut_ranges(&clip, 25.0, &[(2.0, 4.0)]), [(25, 50)]);
}

#[test]
fn a_range_is_cut_only_where_every_clip_covering_it_is_silent() {
    let clips = [
        ((0, 100), vec![(10, 50), (80, 90)]),
        ((40, 200), vec![(40, 45), (60, 120)]),
    ];
    assert_eq!(combine_clip_cuts(&clips), [(10, 45), (80, 90), (100, 120)]);
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tests/fixtures")
        .join(name)
}

/// Synthesized Italian speech: two sentences, with the pauses ffmpeg's
/// `silencedetect` finds at 0–1.04 s, 2.87–4.27 s and 5.41–6.78 s.
#[test]
fn the_pauses_of_real_speech_are_found() {
    let mut analyzed = 0.0;
    let speech = media_speech(&fixture("speech.opus"), 0, |secs| {
        analyzed = secs;
        ControlFlow::Continue(())
    })
    .unwrap();

    assert!((analyzed - 6.78).abs() < 0.05, "{analyzed}");
    assert_spans(
        &silent_spans(&speech, &params(0.3, 0.0, 0.0)),
        &[(0.0, 1.04), (2.87, 4.27), (5.41, 6.78)],
        0.12,
    );
}

#[test]
fn the_analysis_can_be_stopped() {
    let result = media_speech(&fixture("speech.opus"), 0, |_| ControlFlow::Break(()));
    assert_eq!(result.unwrap_err(), "cancelled");
}

#[test]
fn the_speech_of_a_missing_stream_is_an_error() {
    assert_eq!(
        media_speech(&fixture("speech.opus"), 2, |_| ControlFlow::Continue(())).unwrap_err(),
        "the media has no audio stream 2"
    );
}
