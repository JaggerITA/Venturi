//! Audio levels for an agent that decides by itself what is silence.

use std::ops::ControlFlow;
use std::path::Path;

use vv_audio::mixer::{PROJECT_SAMPLE_RATE, timeline_frame_to_sample};
use vv_core::{FrameIdx, Project, Rational, TimelineId};

use crate::export::{ExportError, PROJECT_CHANNELS, mix_audio_track};

/// Below this a window counts as digital silence.
pub const FLOOR_DB: f32 = -120.0;

/// RMS and peak of one window, in dBFS, floored at `FLOOR_DB`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub rms_db: f32,
    pub peak_db: f32,
}

fn to_db(value: f32) -> f32 {
    (20.0 * value.max(1e-9).log10()).max(FLOOR_DB)
}

/// Levels of consecutive windows of `window` frames over `range`, from
/// interleaved samples starting at frame `range.start`; the last window may
/// be shorter.
fn levels(
    samples: &[f32],
    channels: u16,
    rate: u32,
    fps: f64,
    range: std::ops::Range<FrameIdx>,
    window: FrameIdx,
) -> Vec<Level> {
    let base = timeline_frame_to_sample(range.start, fps, rate);
    let at = |frame: FrameIdx| {
        let sample = timeline_frame_to_sample(frame, fps, rate) - base;
        (sample as usize * channels as usize).min(samples.len())
    };
    (range.start..range.end)
        .step_by(window as usize)
        .map(|start| {
            let chunk = &samples[at(start)..at((start + window).min(range.end))];
            let (sum, peak) = chunk.iter().fold((0.0_f64, 0.0_f32), |(sum, peak), &s| {
                (sum + (s as f64) * (s as f64), peak.max(s.abs()))
            });
            let rms = if chunk.is_empty() {
                0.0
            } else {
                (sum / chunk.len() as f64).sqrt() as f32
            };
            Level {
                rms_db: to_db(rms),
                peak_db: to_db(peak),
            }
        })
        .collect()
}

/// Levels of one audio stream of a media file, `range` in media frames at
/// `fps`. Decodes only up to the end of the range.
pub fn media_audio_levels(
    path: &Path,
    stream: usize,
    fps: Rational,
    range: std::ops::Range<FrameIdx>,
    window: FrameIdx,
) -> Result<Vec<Level>, String> {
    let fps = fps.as_f64();
    let end_sample = timeline_frame_to_sample(range.end, fps, PROJECT_SAMPLE_RATE) as usize;
    let start_sample = timeline_frame_to_sample(range.start, fps, PROJECT_SAMPLE_RATE) as usize;
    let mut samples = Vec::new();
    let mut seen = 0usize;
    let mut channels = 1u16;
    let formats = vv_media::decode_audio_streams_streaming(
        path,
        &[stream],
        Some(PROJECT_SAMPLE_RATE),
        |_, chunk_channels, chunk| {
            channels = chunk_channels.max(1);
            let frames = chunk.len() / channels as usize;
            let from = start_sample.saturating_sub(seen).min(frames);
            let to = end_sample.saturating_sub(seen).min(frames);
            samples.extend_from_slice(&chunk[from * channels as usize..to * channels as usize]);
            seen += frames;
            if seen >= end_sample {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )
    .map_err(|e| e.to_string())?;
    if formats.first().copied().flatten().is_none() {
        return Err(format!("the media has no audio stream {stream}"));
    }
    Ok(levels(
        &samples,
        channels,
        PROJECT_SAMPLE_RATE,
        fps,
        range,
        window,
    ))
}

/// Levels of what the timeline plays (all audible tracks mixed, with gains
/// and fades), `range` in timeline frames.
pub fn timeline_audio_levels(
    project: &Project,
    timeline_id: TimelineId,
    range: std::ops::Range<FrameIdx>,
    window: FrameIdx,
) -> Result<Vec<Level>, ExportError> {
    let timeline = project
        .timelines
        .get(timeline_id)
        .ok_or(ExportError::TimelineNotFound)?;
    let mixed = mix_audio_track(project, timeline, range.clone())?;
    Ok(levels(
        &mixed,
        PROJECT_CHANNELS,
        PROJECT_SAMPLE_RATE,
        timeline.fps.as_f64(),
        range,
        window,
    ))
}

#[cfg(test)]
#[path = "tests/analysis.rs"]
mod tests;
