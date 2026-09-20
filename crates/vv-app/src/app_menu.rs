//! Barra dei menu e scorciatoie da tastiera.

use super::*;

impl VibeVideoApp {
    pub(crate) fn handle_shortcuts(&mut self, ui: &mut egui::Ui) {
        // Copia/incolla si gestiscono fuori da `ui.input`: `ctx.copy_text` prende
        // lo stesso lock e dentro andrebbe in deadlock.
        let mut clipboard_events: Vec<egui::Event> = Vec::new();
        let mut arrow_input = (None, 0.0);
        let mut set_fullscreen = None;
        // I tasti scritti in un campo di testo (es. il titolo) non sono
        // scorciatoie: "T" taglierebbe le clip, Backspace le cancellerebbe.
        let typing = ui.ctx().egui_wants_keyboard_input();
        if let Some(timeline_id) = self.timeline_id {
            self.timeline_state
                .drop_locked(&self.project.timelines[timeline_id]);
        }
        let capturing_shortcut = self.settings_dialog.as_ref().is_some_and(|d| d.is_capturing());
        let keymap = self.settings.keymap.clone();
        ui.input(|i| {
            arrow_input.1 = i.time;
            if typing || capturing_shortcut {
                return;
            }
            let pressed = |action| keymap.pressed(action, i);
            arrow_input.0 = match (
                keymap.down(Action::StepBackward, i),
                keymap.down(Action::StepForward, i),
            ) {
                (true, false) => Some(-1),
                (false, true) => Some(1),
                _ => None,
            };
            if pressed(Action::Delete) {
                // Il pannello che ha ricevuto l'ultimo click decide chi
                // cancella: media pool o timeline.
                if self.media_pool_state.focused {
                    self.delete_selected_media();
                } else {
                    self.delete_selected();
                }
            }
            if pressed(Action::RippleDelete) {
                self.ripple_delete_selected();
            }
            if pressed(Action::Split) {
                self.split_at_playhead();
            }
            if pressed(Action::ToggleDisabled) {
                self.toggle_disabled_selected();
            }
            if pressed(Action::Undo) {
                self.history.undo(&mut self.project);
            }
            if pressed(Action::Redo) {
                self.history.redo(&mut self.project);
            }
            if pressed(Action::TogglePlayback) {
                self.toggle_playback();
            }
            if pressed(Action::MarkIn) {
                self.mark_at_playhead(true);
            }
            if pressed(Action::MarkOut) {
                self.mark_at_playhead(false);
            }
            if pressed(Action::FastPlayback) {
                self.handle_fast_playback_key();
            }
            if pressed(Action::SelectAll) {
                // Stessa regola del Canc: il pannello con l'ultimo
                // click decide cosa seleziona Ctrl+A.
                if self.media_pool_state.focused {
                    self.select_all_media();
                } else {
                    self.select_all_clips();
                }
            }
            if pressed(Action::SelectFromPlayhead) {
                self.select_clips_from_playhead();
            }
            if pressed(Action::ImportMedia) {
                self.import_media_dialog();
            }
            if pressed(Action::SaveProject) {
                self.save_project();
            }
            if pressed(Action::SaveProjectAs) {
                self.save_project_as();
            }
            if pressed(Action::OpenProject) {
                self.request_project_switch(ProjectSwitch::Open);
            }
            if pressed(Action::Export) {
                self.start_export();
            }
            // Solo raccolti: gestiti fuori da qui, vedi sopra.
            for (action, event) in [
                (Action::Copy, egui::Event::Copy),
                (Action::Cut, egui::Event::Cut),
                (Action::Paste, egui::Event::Paste(String::new())),
            ] {
                if pressed(action) {
                    clipboard_events.push(event);
                }
            }
            if pressed(Action::ZoomIn) {
                self.timeline_state.zoom_in();
            }
            if pressed(Action::ZoomOut) {
                self.timeline_state.zoom_out();
            }
            if pressed(Action::FullscreenViewer) {
                set_fullscreen = Some(!self.viewer_fullscreen);
            }
            if self.viewer_fullscreen && i.key_pressed(egui::Key::Escape) {
                set_fullscreen = Some(false);
            }
        });
        // Fuori da `ui.input`: `send_viewport_cmd` riprende lo stesso lock.
        if let Some(on) = set_fullscreen.take() {
            self.viewer_fullscreen = on;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
        self.handle_clipboard_events(ui, &clipboard_events);
        if self.step_playhead_with_arrows(arrow_input.0, arrow_input.1) {
            ui.ctx().request_repaint();
        }
    }

    pub(crate) fn show_menu_bar(&mut self, ui: &mut egui::Ui) {
        let keymap = self.settings.keymap.clone();
        let mut set_fullscreen = None;
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let export_disabled = self.timeline_id.is_none()
                    || self.export.is_some()
                    || self.export_dialog.is_some();

                ui.menu_button(t!("menu.file"), |ui| {
                    if ui.button(keymap.menu_label(&t!("menu.open_project"), Action::OpenProject)).clicked() {
                        self.request_project_switch(ProjectSwitch::Open);
                        ui.close();
                    }
                    if ui.button(keymap.menu_label(&t!("menu.save"), Action::SaveProject)).clicked() {
                        self.save_project();
                        ui.close();
                    }
                    if ui.button(keymap.menu_label(&t!("menu.save_as"), Action::SaveProjectAs)).clicked() {
                        self.save_project_as();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(keymap.menu_label(&t!("menu.import_media"), Action::ImportMedia)).clicked() {
                        self.import_media_dialog();
                        ui.close();
                    }
                    if ui
                        .button(t!("menu.import_otio"))
                        .on_hover_text(t!("menu.import_otio_hint"))
                        .clicked()
                    {
                        self.request_project_switch(ProjectSwitch::ImportOtio);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(!export_disabled, egui::Button::new(keymap.menu_label(&t!("menu.export"), Action::Export)))
                        .on_hover_text(t!("menu.export_hint"))
                        .clicked()
                    {
                        self.start_export();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.timeline_id.is_some(), egui::Button::new(t!("menu.export_otio")))
                        .on_hover_text(t!("menu.export_otio_hint"))
                        .clicked()
                    {
                        self.export_otio_dialog();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(t!("menu.settings")).clicked() {
                        self.settings_dialog = Some(settings_dialog::SettingsDialog::new());
                        ui.close();
                    }
                });

                ui.menu_button(t!("menu.edit"), |ui| {
                    if ui.button(keymap.menu_label(&t!("menu.undo"), Action::Undo)).clicked() {
                        self.history.undo(&mut self.project);
                        ui.close();
                    }
                    if ui.button(keymap.menu_label(&t!("menu.redo"), Action::Redo)).clicked() {
                        self.history.redo(&mut self.project);
                        ui.close();
                    }
                    self.undo_history_menu(ui);
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.timeline_state.selected.is_empty(),
                            egui::Button::new(keymap.menu_label(&t!("menu.copy"), Action::Copy)),
                        )
                        .clicked()
                    {
                        // Scrive anche il segnaposto nella clipboard di sistema, vedi
                        // `handle_clipboard_events`.
                        self.handle_clipboard_events(ui, &[egui::Event::Copy]);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.timeline_state.selected.is_empty(),
                            egui::Button::new(keymap.menu_label(&t!("menu.cut"), Action::Cut)),
                        )
                        .clicked()
                    {
                        self.handle_clipboard_events(ui, &[egui::Event::Cut]);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.timeline_state.clipboard.is_empty(),
                            egui::Button::new(keymap.menu_label(&t!("menu.paste"), Action::Paste)),
                        )
                        .on_hover_text(t!("menu.paste_hint"))
                        .clicked()
                    {
                        self.paste_clipboard_at_playhead();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(keymap.menu_label(&t!("menu.delete"), Action::Delete)).clicked() {
                        self.delete_selected();
                        ui.close();
                    }
                    if ui
                        .button(keymap.menu_label(&t!("menu.ripple_delete"), Action::RippleDelete))
                        .on_hover_text(
                            t!("menu.ripple_delete_hint"),
                        )
                        .clicked()
                    {
                        self.ripple_delete_selected();
                        ui.close();
                    }
                    if ui
                        .button(keymap.menu_label(&t!("menu.split"), Action::Split))
                        .on_hover_text(t!("menu.split_hint"))
                        .clicked()
                    {
                        self.split_at_playhead();
                        ui.close();
                    }
                });

                ui.menu_button(t!("menu.timeline"), |ui| {
                    // Checkbox: restano aperti al click, a differenza dei
                    // pulsanti-azione altrove nei menu.
                    ui.checkbox(
                        &mut self.selection_follows_playhead,
                        t!("menu.selection_follows_playhead"),
                    )
                    .on_hover_text(
                        t!("menu.selection_follows_playhead_hint"),
                    );
                    ui.checkbox(&mut self.scrub_audio, t!("menu.scrub_audio"))
                        .on_hover_text(t!("menu.scrub_audio_hint"));
                    ui.separator();
                    if ui.button(keymap.menu_label(&t!("menu.zoom_in"), Action::ZoomIn)).clicked() {
                        self.timeline_state.zoom_in();
                        ui.close();
                    }
                    if ui.button(keymap.menu_label(&t!("menu.zoom_out"), Action::ZoomOut)).clicked() {
                        self.timeline_state.zoom_out();
                        ui.close();
                    }
                    ui.label(t!("menu.zoom_hint"));
                });

                ui.menu_button(t!("menu.playback"), |ui| {
                    ui.menu_button("Proxy", |ui| {
                        if ui
                            .checkbox(&mut self.proxy_enabled, t!("menu.use_proxy"))
                            .on_hover_text(
                                t!("menu.use_proxy_hint"),
                            )
                            .changed()
                        {
                            for render_ahead in self.render_aheads() {
                                render_ahead.set_proxy_enabled(self.proxy_enabled);
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label(t!("menu.read_ahead"));
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.lookahead_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    t!("menu.read_ahead_hint"),
                                )
                                .changed()
                            {
                                for render_ahead in self.render_aheads() {
                                    render_ahead.set_lookahead_secs(self.lookahead_secs);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(t!("menu.read_behind"));
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.behind_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    t!("menu.read_behind_hint"),
                                )
                                .changed()
                            {
                                for render_ahead in self.render_aheads() {
                                    render_ahead.set_behind_secs(self.behind_secs);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(t!("menu.video_cache"));
                            // Espresso in MB nella UI, `cache_budget_bytes` in byte.
                            let mut budget_mb = (self.cache_budget_bytes / 1_000_000) as u32;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut budget_mb)
                                        .range(100..=8000)
                                        .suffix(" MB"),
                                )
                                .on_hover_text(
                                    t!("menu.video_cache_hint"),
                                )
                                .changed()
                            {
                                self.cache_budget_bytes = budget_mb as usize * 1_000_000;
                                for render_ahead in self.render_aheads() {
                                    render_ahead.set_cache_budget_bytes(self.cache_budget_bytes);
                                }
                            }
                        });
                    });
                });

                ui.menu_button(t!("menu.view"), |ui| {
                    ui.checkbox(&mut self.properties_panel_open, t!("menu.inspector"));
                    ui.checkbox(&mut self.audiometer_enabled, t!("menu.audiometer"))
                        .on_hover_text(
                            t!("menu.audiometer_hint"),
                        );
                    ui.separator();
                    if ui.button(keymap.menu_label(&t!("menu.fullscreen_player"), Action::FullscreenViewer)).clicked() {
                        set_fullscreen = Some(true);
                        ui.close();
                    }
                });

                ui.menu_button(t!("menu.help"), |ui| {
                    if ui.button(t!("menu.about")).clicked() {
                        self.about_open = true;
                        ui.close();
                    }
                });
            });
        });

        if let Some(true) = set_fullscreen.take() {
            self.viewer_fullscreen = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
    }

    pub(crate) fn show_about_dialog(&mut self, ctx: &egui::Context) {
        egui::Window::new(t!("about.title"))
            .open(&mut self.about_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("VibeVideo");
                    ui.label(t!("about.version", version = env!("VV_GIT_HASH")));
                    ui.add_space(8.0);
                    ui.label(t!("about.author", author = "Moreno Razzoli a.k.a. Morrolinux"));
                });
            });
    }

    /// Come in Blender: dal più recente, il pallino sullo stato attuale e un
    /// click per saltare a qualunque punto, avanti o indietro.
    fn undo_history_menu(&mut self, ui: &mut egui::Ui) {
        let labels: Vec<vv_core::CommandLabel> = self.history.labels().collect();
        let current = self.history.position();
        let mut jump = None;
        ui.add_enabled_ui(!labels.is_empty(), |ui| {
            ui.menu_button(t!("menu.undo_history"), |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for position in (0..=labels.len()).rev() {
                        let text = match position {
                            0 => t!("history.original"),
                            _ => command_label(labels[position - 1]),
                        };
                        if ui.radio(position == current, text).clicked() {
                            jump = Some(position);
                        }
                    }
                });
            });
        });
        if let Some(position) = jump {
            self.history.go_to(&mut self.project, position);
            ui.close();
        }
    }
}

fn command_label(label: vv_core::CommandLabel) -> std::borrow::Cow<'static, str> {
    use vv_core::CommandLabel as L;
    match label {
        L::AddTrack => t!("history.add_track"),
        L::RemoveTrack => t!("history.remove_track"),
        L::MuteTrack => t!("history.mute_track"),
        L::SoloTrack => t!("history.solo_track"),
        L::LockTrack => t!("history.lock_track"),
        L::ToggleClipsDisabled => t!("history.toggle_clips_disabled"),
        L::InsertClips => t!("history.insert_clips"),
        L::PasteClips => t!("history.paste_clips"),
        L::DuplicateClips => t!("history.duplicate_clips"),
        L::DeleteClips => t!("history.delete_clips"),
        L::RippleDelete => t!("history.ripple_delete"),
        L::MoveClips => t!("history.move_clips"),
        L::TrimClips => t!("history.trim_clips"),
        L::UnlinkClips => t!("history.unlink_clips"),
        L::LinkClips => t!("history.link_clips"),
        L::SplitClips => t!("history.split_clips"),
        L::Fade => t!("history.fade"),
        L::Transform => t!("history.transform"),
        L::Flip => t!("history.flip"),
        L::Gain => t!("history.gain"),
        L::ResetGain => t!("history.reset_gain"),
        L::Title => t!("history.title"),
        L::ResetTransform => t!("history.reset_transform"),
        L::ClipColor => t!("history.clip_color"),
        L::SetKeyframe => t!("history.set_keyframe"),
        L::RemoveKeyframe => t!("history.remove_keyframe"),
        L::RemoveMedia => t!("history.remove_media"),
        L::RelinkMedia => t!("history.relink_media"),
        L::Filters => t!("history.filters"),
        L::Transition => t!("history.transition"),
    }
}
