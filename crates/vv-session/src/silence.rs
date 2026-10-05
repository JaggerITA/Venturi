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
/// The windows at the edges of a speech run this far below its median level
/// are not speech: Silero keeps mouth noises attached to a word, and its
/// probability falls slowly after it.
const EDGE_DROP_DB: f32 = 12.0;

/// Speech probability and RMS level (dBFS) of consecutive `WINDOW_SECS`
/// windows of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechAnalysis {
    pub probabilities: Vec<f32>,
    pub levels_db: Vec<f32>,
}

fn to_db(sum_squares: f64, samples: usize) -> f32 {
    let rms = (sum_squares / samples.max(1) as f64).sqrt() as f32;
    (20.0 * rms.max(1e-9).log10()).max(crate::analysis::FLOOR_DB)
}

/// Decodes the whole stream, downmixed to mono at the model's rate;
/// `on_progress` gets the seconds analyzed so far and can stop the analysis.
pub fn media_speech(
    path: &Path,
    stream: usize,
    mut on_progress: impl FnMut(f64) -> ControlFlow<()>,
) -> Result<SpeechAnalysis, String> {
    let mut vad = Vad::new()?;
    let mut mono = Vec::new();
    let mut frames = 0usize;
    let mut levels_db = Vec::new();
    let (mut sum_squares, mut in_window) = (0.0_f64, 0usize);
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
            for &sample in &mono {
                sum_squares += (sample as f64) * (sample as f64);
                in_window += 1;
                if in_window == vv_vad::CHUNK {
                    levels_db.push(to_db(sum_squares, in_window));
                    (sum_squares, in_window) = (0.0, 0);
                }
            }
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
    if in_window > 0 {
        levels_db.push(to_db(sum_squares, in_window));
    }
    Ok(SpeechAnalysis {
        probabilities: vad.finish()?,
        levels_db,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceParams {
    /// Speech probability, in [0, 1], above which speech starts.
    pub threshold: f32,
    /// Pauses shorter than this (a breath) are kept.
    pub min_silence_secs: f64,
    /// Kept before the speech that ends a pause.
    pub head_secs: f64,
    /// Kept after the speech that precedes a pause.
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
fn speech_runs(analysis: &SpeechAnalysis, threshold: f32) -> Vec<(usize, usize)> {
    let probabilities = &analysis.probabilities;
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
    runs.into_iter()
        .map(|run| trim_quiet_edges(run, &analysis.levels_db))
        .filter(|&(s, e)| e >= s + min_windows)
        .collect()
}

fn trim_quiet_edges((mut start, mut end): (usize, usize), levels_db: &[f32]) -> (usize, usize) {
    let end_level = end.min(levels_db.len());
    let mut levels = levels_db[start.min(end_level)..end_level].to_vec();
    if levels.is_empty() {
        return (start, end);
    }
    levels.sort_by(f32::total_cmp);
    let floor = levels[levels.len() / 2] - EDGE_DROP_DB;
    let quiet = |i: usize| levels_db.get(i).is_some_and(|&db| db < floor);
    while start < end && quiet(start) {
        start += 1;
    }
    while end > start && quiet(end - 1) {
        end -= 1;
    }
    (start, end)
}

/// Spans `[start, end)` of media seconds without speech; the last one runs
/// to infinity, past the end of the audio.
pub fn non_speech_spans(analysis: &SpeechAnalysis, threshold: f32) -> Vec<(f64, f64)> {
    let mut spans = Vec::new();
    let mut previous_end = 0;
    for (start, end) in speech_runs(analysis, threshold) {
        if start > previous_end {
            spans.push((
                previous_end as f64 * WINDOW_SECS,
                start as f64 * WINDOW_SECS,
            ));
        }
        previous_end = end;
    }
    spans.push((previous_end as f64 * WINDOW_SECS, f64::INFINITY));
    spans
}

/// The timeline frames of `clip` that play `spans`, rounded inwards: a
/// frame partly in the speech is kept.
pub fn clip_ranges(clip: &Clip, timeline_fps: f64, spans: &[(f64, f64)]) -> Vec<FrameRange> {
    let frames_per_media_sec = timeline_fps / clip.speed().as_f64();
    let frame_at =
        |secs: f64| (clip.timeline_start - clip.source_offset) as f64 + secs * frames_per_media_sec;
    spans
        .iter()
        .filter_map(|&(start, end)| {
            // `as` saturates the infinite end, then clamped to the clip.
            let start = (frame_at(start).ceil() as FrameIdx).max(clip.timeline_start);
            let end = (frame_at(end).floor() as FrameIdx).min(clip.timeline_end());
            (start < end).then_some((start, end))
        })
        .collect()
}

/// The timeline ranges to remove, from the analyzed clips (their span and
/// their ranges without speech). A pause counts where every clip covering it
/// is without speech, and as a whole even across the cut between two clips;
/// head and tail are kept only next to speech.
pub fn timeline_cuts(
    clips: &[(FrameRange, Vec<FrameRange>)],
    timeline_fps: f64,
    params: &SilenceParams,
) -> Vec<FrameRange> {
    let without_speech = union(clips.iter().flat_map(|(_, ranges)| ranges.iter().copied()));
    let speech = union(
        clips
            .iter()
            .flat_map(|&(span, ref ranges)| subtract(&[span], &union(ranges.iter().copied()))),
    );
    let frames = |secs: f64| (secs * timeline_fps - 1e-6).ceil().max(0.0) as FrameIdx;
    subtract(&without_speech, &speech)
        .into_iter()
        .filter(|&(start, end)| {
            (end - start) as f64 / timeline_fps + 1e-9 >= params.min_silence_secs
        })
        .filter_map(|(mut start, mut end)| {
            if speech.iter().any(|&(_, e)| e == start) {
                start += frames(params.tail_secs);
            }
            if speech.iter().any(|&(s, _)| s == end) {
                end -= frames(params.head_secs);
            }
            (start < end).then_some((start, end))
        })
        .collect()
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
