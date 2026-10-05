//! Silence removal: where a media stream has speech, by voice activity
//! detection, and the timeline ranges without it.

use std::ops::ControlFlow;
use std::path::Path;

use vv_core::{Clip, FrameIdx};
use vv_vad::{SAMPLE_RATE, Vad, WINDOW_SECS};

/// `[start, end)` in timeline frames.
pub type FrameRange = (FrameIdx, FrameIdx);

/// Shorter detections are not speech: a cough, a knock.
const MIN_SPEECH_SECS: f64 = 0.1;
/// Speech, once started, lasts until the probability drops this far below
/// the threshold: a word fading out stays whole (as Silero's own
/// `get_speech_timestamps`).
const HYSTERESIS: f32 = 0.15;

/// Speech probability of consecutive `WINDOW_SECS` windows of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechProbabilities(pub Vec<f32>);

/// Decodes the whole stream, downmixed to mono at the model's rate;
/// `on_progress` gets the seconds analyzed so far and can stop the analysis.
pub fn media_speech(
    path: &Path,
    stream: usize,
    mut on_progress: impl FnMut(f64) -> ControlFlow<()>,
) -> Result<SpeechProbabilities, String> {
    let mut vad = Vad::new()?;
    let mut mono = Vec::new();
    let mut frames = 0usize;
    let mut error = None;
    let mut cancelled = false;
    let formats = vv_media::decode_audio_streams_streaming(
        path,
        &[stream],
        Some(SAMPLE_RATE),
        |_, channels, chunk| {
            let channels = channels.max(1) as usize;
            mono.clear();
            mono.extend(
                chunk
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() / channels as f32),
            );
            if let Err(e) = vad.push(&mono) {
                error = Some(e);
                return ControlFlow::Break(());
            }
            frames += mono.len();
            let flow = on_progress(frames as f64 / SAMPLE_RATE as f64);
            cancelled = flow.is_break();
            flow
        },
    )
    .map_err(|e| e.to_string())?;
    if let Some(e) = error {
        return Err(e);
    }
    if cancelled {
        return Err("cancelled".into());
    }
    if formats.first().copied().flatten().is_none() {
        return Err(format!("the media has no audio stream {stream}"));
    }
    Ok(SpeechProbabilities(vad.finish()?))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceParams {
    /// Speech probability, in [0, 1], above which speech starts.
    pub threshold: f32,
    /// Pauses shorter than this (a breath) are kept.
    pub min_silence_secs: f64,
    /// Kept before the speech that ends a silence.
    pub head_secs: f64,
    /// Kept after the speech that precedes a silence.
    pub tail_secs: f64,
}

impl Default for SilenceParams {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_silence_secs: 0.3,
            head_secs: 0.1,
            tail_secs: 0.1,
        }
    }
}

/// Windows `[start, end)` with speech.
fn speech_runs(probabilities: &[f32], threshold: f32) -> Vec<(usize, usize)> {
    let release = (threshold - HYSTERESIS).max(0.01);
    let min_windows = (MIN_SPEECH_SECS / WINDOW_SECS).ceil() as usize;
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &p) in probabilities.iter().enumerate() {
        match start {
            None if p >= threshold => start = Some(i),
            Some(s) if p < release => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, probabilities.len()));
    }
    runs.retain(|&(s, e)| e - s >= min_windows);
    runs
}

/// Spans `[start, end)` of media seconds to remove, head and tail already
/// left out. Before the first speech and after the last there is nothing to
/// keep a margin for.
pub fn silent_spans(speech: &SpeechProbabilities, params: &SilenceParams) -> Vec<(f64, f64)> {
    let len = speech.0.len();
    let runs = speech_runs(&speech.0, params.threshold);
    let mut gaps = Vec::with_capacity(runs.len() + 1);
    let mut previous_end = 0;
    for &(start, end) in &runs {
        gaps.push((previous_end, start));
        previous_end = end;
    }
    gaps.push((previous_end, len));
    gaps.into_iter()
        .filter(|&(start, end)| end > start)
        .filter_map(|(start, end)| {
            let (from, to) = (start as f64 * WINDOW_SECS, end as f64 * WINDOW_SECS);
            // The epsilon absorbs the rounding of the window multiples.
            if to - from + 1e-9 < params.min_silence_secs {
                return None;
            }
            let from = if start == 0 {
                from
            } else {
                from + params.tail_secs
            };
            let to = if end == len {
                to
            } else {
                to - params.head_secs
            };
            (to > from).then_some((from, to))
        })
        .collect()
}

/// The timeline frames of `clip` that play `spans`, rounded inwards: a
/// frame partly in the sound is kept.
pub fn clip_cut_ranges(
    clip: &Clip,
    timeline_fps: f64,
    spans: &[(f64, f64)],
) -> Vec<(FrameIdx, FrameIdx)> {
    let frames_per_media_sec = timeline_fps / clip.speed().as_f64();
    let frame_at =
        |secs: f64| (clip.timeline_start - clip.source_offset) as f64 + secs * frames_per_media_sec;
    spans
        .iter()
        .filter_map(|&(start, end)| {
            let start = (frame_at(start).ceil() as FrameIdx).max(clip.timeline_start);
            let end = (frame_at(end).floor() as FrameIdx).min(clip.timeline_end());
            (start < end).then_some((start, end))
        })
        .collect()
}

/// Of the analyzed clips (their span and the cuts inside it), the ranges
/// that are silent on every clip covering them: what one track hears is not
/// cut because another is quiet there.
pub fn combine_clip_cuts(clips: &[(FrameRange, Vec<FrameRange>)]) -> Vec<(FrameIdx, FrameIdx)> {
    let cuts = union(clips.iter().flat_map(|(_, cuts)| cuts.iter().copied()));
    let kept = union(
        clips
            .iter()
            .flat_map(|&(span, ref cuts)| subtract(&[span], &union(cuts.iter().copied()))),
    );
    subtract(&cuts, &kept)
}

fn union(ranges: impl Iterator<Item = (FrameIdx, FrameIdx)>) -> Vec<(FrameIdx, FrameIdx)> {
    let mut ranges: Vec<_> = ranges.filter(|&(s, e)| e > s).collect();
    ranges.sort();
    let mut merged: Vec<(FrameIdx, FrameIdx)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Both sorted and disjoint.
fn subtract(
    from: &[(FrameIdx, FrameIdx)],
    holes: &[(FrameIdx, FrameIdx)],
) -> Vec<(FrameIdx, FrameIdx)> {
    let mut out = Vec::new();
    for &(start, end) in from {
        let mut cursor = start;
        for &(hole_start, hole_end) in holes {
            if hole_end <= cursor || hole_start >= end {
                continue;
            }
            if hole_start > cursor {
                out.push((cursor, hole_start));
            }
            cursor = cursor.max(hole_end);
        }
        if cursor < end {
            out.push((cursor, end));
        }
    }
    out
}

#[cfg(test)]
#[path = "tests/silence.rs"]
mod tests;
