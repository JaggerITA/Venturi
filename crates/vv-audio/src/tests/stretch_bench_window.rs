use super::*;
use std::time::{Duration, Instant};

/// Not a hard-realtime guarantee, but a generous threshold: stretching the
/// window chosen by `vv-app` (`WINDOW_FRAMES` in `timeline_audio`, 8s) must stay
/// well under the real margin available before the background extension
/// (triggered when `EXTEND_TRIGGER_MARGIN_SECS`, 4s, of window are left
/// unplayed) is actually needed — that margin in *real* time shrinks
/// with the speed (4s of original audio are 1s real at 4x, 0.5s at 8x),
/// so the threshold here scales accordingly (half the real margin, 2x
/// headroom). Measured in practice at ~150ms regardless of tempo, see
/// the module comment.
#[test]
fn stretching_an_8s_window_is_well_under_the_extension_margin() {
    let sample_rate = 48_000u32;
    let channels = 2u16;
    let secs = 8.0;
    let n = (sample_rate as f64 * secs) as usize;
    let samples: Vec<f32> = (0..n * channels as usize)
        .map(|i| (i as f32 * 0.01).sin())
        .collect();
    const EXTEND_TRIGGER_MARGIN_SECS: f64 = 4.0;
    for tempo in [2.0, 4.0, 8.0] {
        let real_margin_secs = EXTEND_TRIGGER_MARGIN_SECS / tempo;
        let threshold = Duration::from_secs_f64(real_margin_secs / 2.0);
        let start = Instant::now();
        stretch_samples(&samples, sample_rate, channels, tempo).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < threshold,
            "tempo={tempo} elapsed={elapsed:?} threshold={threshold:?}, too slow for background extension at this speed"
        );
    }
}
