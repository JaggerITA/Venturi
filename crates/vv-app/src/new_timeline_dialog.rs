//! Settings window of a new timeline (name, frame rate, resolution).

use super::*;

const FPS_CHOICES: [vv_core::Rational; 8] = [
    vv_core::Rational::new(24000, 1001),
    vv_core::Rational::new(24, 1),
    vv_core::Rational::new(25, 1),
    vv_core::Rational::new(30000, 1001),
    vv_core::Rational::new(30, 1),
    vv_core::Rational::new(50, 1),
    vv_core::Rational::new(60000, 1001),
    vv_core::Rational::new(60, 1),
];

const RESOLUTION_CHOICES: [(u32, u32); 5] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (1080, 1920),
];

pub(crate) struct NewTimelineDialog {
    pub name: String,
    pub fps: vv_core::Rational,
    pub width: u32,
    pub height: u32,
    custom_resolution: bool,
}

impl NewTimelineDialog {
    pub(crate) fn new(name: String, fps: vv_core::Rational, resolution: (u32, u32)) -> Self {
        Self {
            name,
            fps,
            width: resolution.0,
            height: resolution.1,
            custom_resolution: !RESOLUTION_CHOICES.contains(&resolution),
        }
    }
}

pub(crate) enum NewTimelineAction {
    None,
    Cancel,
    Create,
}

impl VenturiApp {
    pub(crate) fn show_new_timeline_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.new_timeline_dialog else {
            return;
        };
        let mut action = NewTimelineAction::None;
        let mut open = true;
        egui::Window::new(t!("pool.new_timeline_title"))
            .id(egui::Id::new("new_timeline_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                egui::Grid::new("new_timeline_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(t!("pool.timeline_name"));
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.name).desired_width(220.0),
                        );
                        ui.end_row();

                        ui.label(t!("pool.timeline_fps"));
                        let items: Vec<_> = FPS_CHOICES
                            .iter()
                            .map(|fps| (*fps, fps_label(*fps), true))
                            .collect();
                        preview_combo(
                            ui,
                            "new_timeline_fps",
                            &mut dialog.fps,
                            &items,
                            Some(220.0),
                            None,
                            None,
                        );
                        ui.end_row();

                        ui.label(t!("pool.timeline_resolution"));
                        let mut choice = if dialog.custom_resolution {
                            None
                        } else {
                            Some((dialog.width, dialog.height))
                        };
                        let mut items: Vec<_> = RESOLUTION_CHOICES
                            .iter()
                            .map(|(w, h)| (Some((*w, *h)), format!("{w}×{h}"), true))
                            .collect();
                        items.push((None, t!("pool.resolution_custom").into_owned(), true));
                        if preview_combo(
                            ui,
                            "new_timeline_resolution",
                            &mut choice,
                            &items,
                            Some(220.0),
                            None,
                            None,
                        ) {
                            match choice {
                                Some((w, h)) => {
                                    dialog.custom_resolution = false;
                                    dialog.width = w;
                                    dialog.height = h;
                                }
                                None => dialog.custom_resolution = true,
                            }
                        }
                        ui.end_row();

                        if dialog.custom_resolution {
                            ui.label("");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut dialog.width).range(16..=16384));
                                ui.label("×");
                                ui.add(egui::DragValue::new(&mut dialog.height).range(16..=16384));
                            });
                            ui.end_row();
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !dialog.name.trim().is_empty(),
                            egui::Button::new(t!("pool.create_timeline")),
                        )
                        .clicked()
                    {
                        action = NewTimelineAction::Create;
                    }
                    if ui.button(t!("common.cancel")).clicked() {
                        action = NewTimelineAction::Cancel;
                    }
                });
            });
        if !open {
            action = NewTimelineAction::Cancel;
        }
        match action {
            NewTimelineAction::None => {}
            NewTimelineAction::Cancel => self.new_timeline_dialog = None,
            NewTimelineAction::Create => {
                let dialog = self.new_timeline_dialog.take().expect("just checked");
                let id = self.create_timeline(
                    dialog.name.trim().to_string(),
                    dialog.fps,
                    (dialog.width.max(2) & !1, dialog.height.max(2) & !1),
                );
                self.open_timeline(id);
            }
        }
    }

    /// Default settings of a new timeline: those of the current one, so
    /// the second timeline of a project matches the first.
    pub(crate) fn open_new_timeline_dialog(&mut self) {
        let (fps, resolution) = match self.timeline_id {
            Some(id) => {
                let tl = &self.project.timelines[id];
                (tl.fps, tl.resolution)
            }
            None => (vv_core::Rational::new(25, 1), (1920, 1080)),
        };
        self.new_timeline_dialog = Some(NewTimelineDialog::new(
            self.project.alloc_timeline_name(),
            fps,
            resolution,
        ));
    }
}

fn fps_label(fps: vv_core::Rational) -> String {
    if fps.den == 1 {
        format!("{}", fps.num)
    } else {
        format!("{:.2}", fps.as_f64())
    }
}
