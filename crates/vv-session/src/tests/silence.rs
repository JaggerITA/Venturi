use super::*;
use std::ops::ControlFlow;
use vv_core::{ClipId, ClipSource, Id, MediaId, Rational};

const W: f64 = WINDOW_SECS;
const INF: f64 = f64::INFINITY;

/// `(probability, level in dB, windows)` per entry.
fn analysis(runs: &[(f32, f32, usize)]) -> SpeechAnalysis {
    let repeat = |pick: fn(&(f32, f32, usize)) -> f32| {
        runs.iter()
            .flat_map(|run| std::iter::repeat_n(pick(run), run.2))
            .collect()
    };
    SpeechAnalysis {
        probabilities: repeat(|r| r.0),
        levels_db: repeat(|r| r.1),
    }
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
    let close = |a: f64, e: f64| a == e || (a - e).abs() <= tolerance;
    assert_eq!(actual.len(), expected.len(), "{actual:?} != {expected:?}");
    for (a, e) in actual.iter().zip(expected) {
        assert!(
            close(a.0, e.0) && close(a.1, e.1),
            "{actual:?} != {expected:?}"
        );
    }
}

#[test]
fn spans_are_the_gaps_between_speech_and_the_rest_after_it() {
    let speech = analysis(&[(0.9, -20.0, 30), (0.0, -60.0, 20), (0.9, -20.0, 30)]);
    assert_spans(
        &non_speech_spans(&speech, 0.5),
        &[(30.0 * W, 50.0 * W), (80.0 * W, INF)],
        1e-9,
    );
}

#[test]
fn speech_fading_out_lasts_until_well_under_the_threshold() {
    let speech = analysis(&[
        (0.9, -20.0, 30),
        (0.4, -20.0, 10),
        (0.0, -60.0, 20),
        (0.9, -20.0, 30),
    ]);
    assert_spans(
        &non_speech_spans(&speech, 0.5),
        &[(40.0 * W, 60.0 * W), (90.0 * W, INF)],
        1e-9,
    );
}

#[test]
fn a_detection_too_short_for_speech_is_not_speech() {
    let speech = analysis(&[
        (0.0, -60.0, 30),
        (0.9, -20.0, 2),
        (0.0, -60.0, 30),
        (0.9, -20.0, 30),
    ]);
    assert_spans(
        &non_speech_spans(&speech, 0.5),
        &[(0.0, 62.0 * W), (92.0 * W, INF)],
        1e-9,
    );
}

#[test]
fn quiet_windows_at_the_edges_of_speech_are_not_speech() {
    let speech = analysis(&[(0.9, -40.0, 5), (0.9, -20.0, 30), (0.9, -40.0, 5)]);
    assert_spans(
        &non_speech_spans(&speech, 0.5),
        &[(0.0, 5.0 * W), (35.0 * W, INF)],
        1e-9,
    );
}

#[test]
fn a_quiet_moment_inside_speech_stays_speech() {
    let speech = analysis(&[(0.9, -20.0, 10), (0.9, -40.0, 5), (0.9, -20.0, 10)]);
    assert_spans(&non_speech_spans(&speech, 0.5), &[(25.0 * W, INF)], 1e-9);
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
fn clip_ranges_follow_the_clip_position_and_trim() {
    // Media second 2 is frame 50 of the source, trimmed from frame 25.
    let clip = clip(100, 25, 100, Rational::one());
    assert_eq!(clip_ranges(&clip, 25.0, &[(2.0, 3.0)]), [(125, 150)]);
}

#[test]
fn clip_ranges_round_inwards_and_stay_in_the_clip() {
    let clip = clip(0, 0, 50, Rational::one());
    assert_eq!(
        clip_ranges(&clip, 25.0, &[(0.01, 0.99), (1.5, INF), (5.0, 6.0)]),
        [(1, 24), (38, 50)]
    );
}

#[test]
fn clip_ranges_follow_the_clip_speed() {
    let clip = clip(0, 0, 100, Rational::new(2, 1));
    assert_eq!(clip_ranges(&clip, 25.0, &[(2.0, 4.0)]), [(25, 50)]);
}

#[test]
fn pauses_shorter_than_the_minimum_are_kept() {
    let clips = [((0, 100), vec![(20, 25), (50, 100)])];
    assert_eq!(
        timeline_cuts(&clips, 25.0, &params(0.3, 0.0, 0.0)),
        [(50, 100)]
    );
}

#[test]
fn a_pause_across_the_cut_between_two_clips_counts_as_a_whole() {
    let clips = [((0, 100), vec![(95, 100)]), ((100, 200), vec![(100, 105)])];
    assert_eq!(
        timeline_cuts(&clips, 25.0, &params(0.3, 0.0, 0.0)),
        [(95, 105)]
    );
}

#[test]
fn a_range_is_cut_only_where_every_clip_covering_it_is_without_speech() {
    let clips = [
        ((0, 100), vec![(10, 50), (80, 90)]),
        ((40, 200), vec![(40, 45), (60, 120)]),
    ];
    assert_eq!(
        timeline_cuts(&clips, 25.0, &params(0.0, 0.0, 0.0)),
        [(10, 45), (80, 90), (100, 120)]
    );
}

#[test]
fn head_and_tail_are_kept_only_next_to_speech() {
    let clips = [((0, 100), vec![(0, 20), (40, 60), (90, 100)])];
    assert_eq!(
        timeline_cuts(&clips, 25.0, &params(0.0, 0.08, 0.04)),
        [(0, 18), (41, 58), (91, 100)]
    );
}

#[test]
fn a_pause_eaten_by_head_and_tail_disappears() {
    let clips = [((0, 100), vec![(40, 43)])];
    assert!(timeline_cuts(&clips, 25.0, &params(0.0, 0.08, 0.04)).is_empty());
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
    assert_eq!(speech.levels_db.len(), speech.probabilities.len());
    let pauses: Vec<_> = non_speech_spans(&speech, 0.5)
        .into_iter()
        .filter(|&(start, end)| end - start >= 0.3)
        .collect();
    assert_spans(&pauses, &[(0.0, 1.04), (2.87, 4.27), (5.41, INF)], 0.12);
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
