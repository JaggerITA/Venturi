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

                ui.menu_button("File", |ui| {
                    if ui.button(keymap.menu_label("Apri progetto...", Action::OpenProject)).clicked() {
                        self.request_project_switch(ProjectSwitch::Open);
                        ui.close();
                    }
                    if ui.button(keymap.menu_label("Salva", Action::SaveProject)).clicked() {
                        self.save_project();
                        ui.close();
                    }
                    if ui.button(keymap.menu_label("Salva con nome...", Action::SaveProjectAs)).clicked() {
                        self.save_project_as();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(keymap.menu_label("Importa media...", Action::ImportMedia)).clicked() {
                        self.import_media_dialog();
                        ui.close();
                    }
                    if ui
                        .button("Importa OTIO...")
                        .on_hover_text("Apre una timeline OpenTimelineIO come nuovo progetto")
                        .clicked()
                    {
                        self.request_project_switch(ProjectSwitch::ImportOtio);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(!export_disabled, egui::Button::new(keymap.menu_label("Esporta...", Action::Export)))
                        .on_hover_text("Esporta la timeline tra in e out (tasti I/O) in un file MP4 (H.264 + AAC)")
                        .clicked()
                    {
                        self.start_export();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.timeline_id.is_some(), egui::Button::new("Esporta OTIO..."))
                        .on_hover_text("Esporta la timeline in OpenTimelineIO, per aprirla in un altro editor")
                        .clicked()
                    {
                        self.export_otio_dialog();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Impostazioni...").clicked() {
                        self.settings_dialog = Some(settings_dialog::SettingsDialog::new());
                        ui.close();
                    }
                });

                ui.menu_button("Modifica", |ui| {
                    if ui.button(keymap.menu_label("Undo", Action::Undo)).clicked() {
                        self.history.undo(&mut self.project);
                        ui.close();
                    }
                    if ui.button(keymap.menu_label("Redo", Action::Redo)).clicked() {
                        self.history.redo(&mut self.project);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.timeline_state.selected.is_empty(),
                            egui::Button::new(keymap.menu_label("Copia", Action::Copy)),
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
                            egui::Button::new(keymap.menu_label("Taglia", Action::Cut)),
                        )
                        .clicked()
                    {
                        self.handle_clipboard_events(ui, &[egui::Event::Cut]);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.timeline_state.clipboard.is_empty(),
                            egui::Button::new(keymap.menu_label("Incolla", Action::Paste)),
                        )
                        .on_hover_text("Incolla alla posizione del playhead")
                        .clicked()
                    {
                        self.paste_clipboard_at_playhead();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(keymap.menu_label("Elimina", Action::Delete)).clicked() {
                        self.delete_selected();
                        ui.close();
                    }
                    if ui
                        .button(keymap.menu_label("Ripple delete", Action::RippleDelete))
                        .on_hover_text(
                            "Rimuove la clip e chiude il gap su tutte le track, mantenendo il sync A/V",
                        )
                        .clicked()
                    {
                        self.ripple_delete_selected();
                        ui.close();
                    }
                    if ui
                        .button(keymap.menu_label("Dividi", Action::Split))
                        .on_hover_text("Taglia al playhead le clip selezionate, o tutte se non c'è selezione")
                        .clicked()
                    {
                        self.split_at_playhead();
                        ui.close();
                    }
                });

                ui.menu_button("Timeline", |ui| {
                    // Checkbox: restano aperti al click, a differenza dei
                    // pulsanti-azione altrove nei menu.
                    ui.checkbox(
                        &mut self.selection_follows_playhead,
                        "Selection follows playhead",
                    )
                    .on_hover_text(
                        "Sposta la selezione sulla clip video sotto al playhead a ogni scrub/taglio/ripple-delete",
                    );
                    ui.checkbox(&mut self.scrub_audio, "Audio durante lo scrub")
                        .on_hover_text("Suona un breve frammento audio a ogni spostamento manuale del playhead");
                    ui.separator();
                    if ui.button(keymap.menu_label("Zoom avanti", Action::ZoomIn)).clicked() {
                        self.timeline_state.zoom_in();
                        ui.close();
                    }
                    if ui.button(keymap.menu_label("Zoom indietro", Action::ZoomOut)).clicked() {
                        self.timeline_state.zoom_out();
                        ui.close();
                    }
                    ui.label("Alt+scroll (o pinch) sopra la timeline zooma allo stesso modo.");
                });

                ui.menu_button("Playback", |ui| {
                    ui.menu_button("Proxy", |ui| {
                        if ui
                            .checkbox(&mut self.proxy_enabled, "Usa proxy")
                            .on_hover_text(
                                "Anteprima/editing da una copia a bassa risoluzione generata in \
                                 background invece che dal sorgente: scrub molto più fluido su \
                                 sorgenti lunghi. L'export non è mai influenzato, usa sempre i \
                                 sorgenti originali. Disattiva per lavori che richiedono la \
                                 qualità piena.",
                            )
                            .changed()
                        {
                            for render_ahead in self.render_aheads() {
                                render_ahead.set_proxy_enabled(self.proxy_enabled);
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label("Read-ahead avanti:");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.lookahead_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    "Quanti secondi di timeline bufferizzare in anticipo avanti \
                                     dalla testina. Di più = scrub/playback più fluidi ma più RAM \
                                     e CPU spesi su frame che potrebbero non servire mai; di meno \
                                     = più leggero ma più probabile una breve attesa durante uno \
                                     scrub veloce. Resta comunque un margine minimo anche a 0.",
                                )
                                .changed()
                            {
                                for render_ahead in self.render_aheads() {
                                    render_ahead.set_lookahead_secs(self.lookahead_secs);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Read-ahead dietro:");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.behind_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    "Quanti secondi di timeline tenere bufferizzati anche dietro \
                                     la testina, oltre alla finestra in avanti: rende economico \
                                     uno scrub avanti-indietro ravvicinato senza dover \
                                     ridecodificare ogni volta. Resta comunque un margine minimo \
                                     anche a 0.",
                                )
                                .changed()
                            {
                                for render_ahead in self.render_aheads() {
                                    render_ahead.set_behind_secs(self.behind_secs);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Cache video:");
                            // Espresso in MB nella UI, `cache_budget_bytes` in byte.
                            let mut budget_mb = (self.cache_budget_bytes / 1_000_000) as u32;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut budget_mb)
                                        .range(100..=8000)
                                        .suffix(" MB"),
                                )
                                .on_hover_text(
                                    "Quanta RAM usare per i frame pre-decodificati: di più = \
                                     scrub/playback più fluidi, di meno = meno rischio di esaurire \
                                     la memoria (soprattutto con sorgenti 4K+).",
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

                ui.menu_button("Visualizza", |ui| {
                    ui.checkbox(&mut self.properties_panel_open, "Inspector");
                    ui.checkbox(&mut self.audiometer_enabled, "Audiometer")
                        .on_hover_text(
                            "Livello del player attivo, in una fascia stretta a destra della timeline",
                        );
                    ui.separator();
                    if ui.button(keymap.menu_label("Player a schermo intero", Action::FullscreenViewer)).clicked() {
                        set_fullscreen = Some(true);
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
}
