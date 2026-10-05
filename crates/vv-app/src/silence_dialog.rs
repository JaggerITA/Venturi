//! Silence removal: the dialog, the background speech detection and the red
//! preview of the cuts on the timeline.

use std::ops::ControlFlow;

use super::*;
use vv_session::silence::{self, SpeechProbabilities};

/// `(content_hash, audio stream)`.
type StreamKey = (u64, usize);

enum AnalysisEvent {
    /// Seconds analyzed of the stream in progress.
    Progress(f64),
    Done(StreamKey, Result<SpeechProbabilities, String>),
}

pub(crate) struct SilenceDialog {
    timeline_id: TimelineId,
    /// Audio clips with a media source.
    targets: Vec<(usize, ClipId)>,
    analyzing: usize,
    /// Seconds of audio to analyze, and analyzed so far.
    total_secs: f64,
    done_secs: f64,
    current_secs: f64,
    events: mpsc::Receiver<AnalysisEvent>,
    error: Option<String>,
}

impl VenturiApp {
    pub(crate) fn open_silence_dialog(
        &mut self,
        ctx: &egui::Context,
        targets: Vec<(usize, ClipId)>,
    ) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let project = &self.session.project;
        let tl = &project.timelines[timeline_id];
        // A selected video clip stands for the audio linked to it.
        let groups: Vec<_> = targets
            .iter()
            .filter_map(|&(track, id)| tl.clip(track, id)?.linked_group)
            .collect();
        let mut audio = Vec::new();
        for (track_index, track) in tl.tracks.iter().enumerate() {
            if track.kind != TrackKind::Audio {
                continue;
            }
            for c in &track.clips {
                let picked = targets.contains(&(track_index, c.id))
                    || c.linked_group.is_some_and(|g| groups.contains(&g));
                if picked && matches!(c.source, vv_core::ClipSource::Media(_)) {
                    audio.push((track_index, c.id));
                }
            }
        }
        let targets = audio;
        if targets.is_empty() {
            return;
        }
        let mut missing: Vec<(StreamKey, PathBuf, f64)> = Vec::new();
        for &(track, id) in &targets {
            let clip = tl.clip(track, id).unwrap();
            let vv_core::ClipSource::Media(media) = clip.source else {
                continue;
            };
            let Some(item) = project.media_pool.get(media) else {
                continue;
            };
            let key = (item.content_hash, clip.audio_stream_index);
            if !self.silence_speech.contains_key(&key) && !missing.iter().any(|m| m.0 == key) {
                let secs = item.meta.duration_frames as f64 / item.meta.fps.as_f64();
                missing.push((key, item.path.clone(), secs));
            }
        }
        let (tx, events) = mpsc::channel();
        let analyzing = missing.len();
        let total_secs = missing.iter().map(|m| m.2).sum();
        if !missing.is_empty() {
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                for (key, path, _) in missing {
                    let mut reported = 0.0;
                    // Stops when the dialog is closed and drops the receiver.
                    let speech = silence::media_speech(&path, key.1, |secs| {
                        if secs - reported < 1.0 {
                            return ControlFlow::Continue(());
                        }
                        reported = secs;
                        ctx.request_repaint();
                        match tx.send(AnalysisEvent::Progress(secs)) {
                            Ok(()) => ControlFlow::Continue(()),
                            Err(_) => ControlFlow::Break(()),
                        }
                    });
                    if tx.send(AnalysisEvent::Done(key, speech)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }
        self.silence_dialog = Some(SilenceDialog {
            timeline_id,
            targets,
            analyzing,
            total_secs,
            done_secs: 0.0,
            current_secs: 0.0,
            events,
            error: None,
        });
    }

    /// The timeline ranges to remove, `None` while the analysis runs.
    fn silence_cuts(&self, dialog: &SilenceDialog) -> Option<Vec<(FrameIdx, FrameIdx)>> {
        if dialog.analyzing > 0 || dialog.error.is_some() {
            return None;
        }
        let project = &self.session.project;
        let tl = project.timelines.get(dialog.timeline_id)?;
        let fps = tl.fps.as_f64();
        let clips: Vec<_> = dialog
            .targets
            .iter()
            .filter_map(|&(track, id)| {
                let clip = tl.clip(track, id)?;
                let vv_core::ClipSource::Media(media) = clip.source else {
                    return None;
                };
                let item = project.media_pool.get(media)?;
                let speech = self
                    .silence_speech
                    .get(&(item.content_hash, clip.audio_stream_index))?;
                let spans = silence::silent_spans(speech, &self.silence_params);
                Some((
                    (clip.timeline_start, clip.timeline_end()),
                    silence::clip_cut_ranges(clip, fps, &spans),
                ))
            })
            .collect();
        Some(silence::combine_clip_cuts(&clips))
    }

    pub(crate) fn show_silence_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.silence_dialog else {
            return;
        };
        for event in dialog.events.try_iter() {
            match event {
                AnalysisEvent::Progress(secs) => dialog.current_secs = secs,
                AnalysisEvent::Done(key, result) => {
                    dialog.analyzing = dialog.analyzing.saturating_sub(1);
                    dialog.done_secs += dialog.current_secs;
                    dialog.current_secs = 0.0;
                    match result {
                        Ok(speech) => {
                            self.silence_speech.insert(key, speech);
                        }
                        Err(e) => dialog.error = Some(e),
                    }
                }
            }
        }
        if self.timeline_id != Some(dialog.timeline_id) {
            self.close_silence_dialog();
            return;
        }
        let dialog = self.silence_dialog.as_ref().unwrap();
        let cuts = self.silence_cuts(dialog);
        let fps = self.session.project.timelines[dialog.timeline_id]
            .fps
            .as_f64();
        let analyzing = dialog.analyzing > 0;
        let progress =
            ((dialog.done_secs + dialog.current_secs) / dialog.total_secs.max(1e-3)) as f32;
        let error = dialog.error.clone();
        self.timeline_state.silence_preview = cuts.clone().unwrap_or_default();

        let params = &mut self.silence_params;
        let mut open = true;
        let (mut apply, mut cancel) = (false, false);
        egui::Window::new(t!("silence.title"))
            .id(egui::Id::new("silence_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("silence_params")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label(t!("silence.threshold"))
                            .on_hover_text(t!("silence.threshold_hint"));
                        let mut percent = params.threshold * 100.0;
                        if ui
                            .add(
                                egui::Slider::new(&mut percent, 10.0..=90.0)
                                    .step_by(1.0)
                                    .suffix("%"),
                            )
                            .changed()
                        {
                            params.threshold = percent / 100.0;
                        }
                        ui.end_row();
                        for (label, hint, secs, max_ms) in [
                            (
                                t!("silence.min_silence"),
                                t!("silence.min_silence_hint"),
                                &mut params.min_silence_secs,
                                3000.0,
                            ),
                            (
                                t!("silence.head"),
                                t!("silence.head_hint"),
                                &mut params.head_secs,
                                1000.0,
                            ),
                            (
                                t!("silence.tail"),
                                t!("silence.tail_hint"),
                                &mut params.tail_secs,
                                1000.0,
                            ),
                        ] {
                            ui.label(label).on_hover_text(hint);
                            let mut ms = *secs * 1000.0;
                            if ui
                                .add(
                                    egui::Slider::new(&mut ms, 0.0..=max_ms)
                                        .step_by(10.0)
                                        .suffix(" ms"),
                                )
                                .changed()
                            {
                                *secs = ms / 1000.0;
                            }
                            ui.end_row();
                        }
                    });
                ui.separator();
                match (&error, &cuts) {
                    (Some(e), _) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            t!("silence.failed", error = e),
                        );
                    }
                    (None, Some(cuts)) => {
                        let frames: FrameIdx = cuts.iter().map(|&(s, e)| e - s).sum();
                        ui.label(t!(
                            "silence.summary",
                            count = cuts.len(),
                            secs = format!("{:.1}", frames as f64 / fps)
                        ));
                    }
                    (None, None) => {
                        ui.label(t!("silence.analyzing"));
                        ui.add(egui::ProgressBar::new(progress.min(1.0)).show_percentage());
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button(t!("common.cancel")).clicked() {
                        cancel = true;
                    }
                    let can_apply = !analyzing && cuts.as_ref().is_some_and(|c| !c.is_empty());
                    if ui
                        .add_enabled(can_apply, egui::Button::new(t!("silence.apply")))
                        .clicked()
                    {
                        apply = true;
                    }
                });
            });
        if !open {
            cancel = true;
        }
        if apply && let Some(cuts) = cuts {
            let timeline_id = self.silence_dialog.as_ref().unwrap().timeline_id;
            let history = &mut self.session.history;
            let mark = history.begin_group();
            vv_core::edit::delete_ranges(
                &mut self.session.project,
                history,
                timeline_id,
                &cuts,
                &vv_core::edit::RangeDelete::Ripple,
            );
            history.end_group_as(mark, vv_core::CommandLabel::RemoveSilences);
            // The splits leave the selection on just a piece of each clip.
            self.timeline_state.clear_selection();
        }
        if apply || cancel {
            self.close_silence_dialog();
        }
    }

    fn close_silence_dialog(&mut self) {
        self.silence_dialog = None;
        self.timeline_state.silence_preview.clear();
    }
}
