//! Clip speed: "Change Clip Speed…" dialog and the Ctrl+R retime controls.

use super::*;
use timeline_ui::SPEED_PERCENT_RANGE;

pub(crate) struct SpeedDialog {
    targets: Vec<(usize, ClipId)>,
    percent: f64,
    pitch_correction: bool,
    ripple: bool,
}

impl VenturiApp {
    /// Shows the retime bar on the selected media clips, or hides it if they
    /// all have it already.
    pub(crate) fn toggle_retime_controls(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let tl = &self.project.timelines[timeline_id];
        let ids: Vec<ClipId> = self
            .timeline_state
            .selected
            .iter()
            .filter(|&&(track, id)| {
                tl.clip(track, id)
                    .is_some_and(|c| matches!(c.source, vv_core::ClipSource::Media(_)))
            })
            .map(|&(_, id)| id)
            .collect();
        let controls = &mut self.timeline_state.retime_controls;
        if ids.iter().all(|id| controls.contains(id)) {
            for id in &ids {
                controls.remove(id);
            }
        } else {
            controls.extend(ids);
        }
    }

    pub(crate) fn open_speed_dialog(&mut self, targets: Vec<(usize, ClipId)>) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(first) = targets
            .iter()
            .find_map(|&(track, id)| self.project.timelines[timeline_id].clip(track, id))
        else {
            return;
        };
        self.speed_dialog = Some(SpeedDialog {
            percent: first.speed.as_percent(),
            pitch_correction: first.pitch_correction,
            ripple: true,
            targets,
        });
    }

    pub(crate) fn show_speed_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.speed_dialog else {
            return;
        };
        let mut open = true;
        let (mut apply, mut cancel) = (false, false);
        egui::Window::new(t!("speed.title"))
            .id(egui::Id::new("speed_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t!("speed.speed"));
                    ui.add(
                        egui::DragValue::new(&mut dialog.percent)
                            .range(SPEED_PERCENT_RANGE)
                            .speed(1.0)
                            .max_decimals(2)
                            .suffix("%"),
                    );
                });
                ui.checkbox(&mut dialog.pitch_correction, t!("speed.pitch_correction"))
                    .on_hover_text(t!("speed.pitch_correction_hint"));
                ui.checkbox(&mut dialog.ripple, t!("speed.ripple"))
                    .on_hover_text(t!("speed.ripple_hint"));
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t!("common.cancel")).clicked() {
                        cancel = true;
                    }
                    if ui.button(t!("speed.apply")).clicked() {
                        apply = true;
                    }
                });
            });
        if !open {
            cancel = true;
        }
        if !(apply || cancel) {
            return;
        }
        let dialog = self.speed_dialog.take().unwrap();
        if cancel {
            return;
        }
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::SetClipSpeed::new(
                timeline_id,
                dialog.targets,
                vv_core::Rational::from_percent(dialog.percent),
                dialog.pitch_correction,
                dialog.ripple,
            )),
        );
    }
}
