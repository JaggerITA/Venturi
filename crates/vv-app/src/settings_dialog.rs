//! File → Settings window.

use std::borrow::Cow;

use vv_media::proxy::ProxyQuality;

use crate::i18n::Language;
use crate::properties_panel::preview_combo;
use crate::settings::{Action, Keymap, Settings, Shortcut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Playback,
    Shortcuts,
}

impl Section {
    const ALL: [Section; 3] = [Section::General, Section::Playback, Section::Shortcuts];

    fn title(self) -> Cow<'static, str> {
        match self {
            Section::General => t!("settings.section_general"),
            Section::Playback => t!("settings.section_playback"),
            Section::Shortcuts => t!("settings.section_shortcuts"),
        }
    }
}

/// Shortcut waiting for the next key: replaces the `slot`-th one, or is
/// appended if `None`.
#[derive(Debug, Clone, Copy)]
struct Capture {
    action: Action,
    slot: Option<usize>,
}

pub struct SettingsDialog {
    section: Section,
    capture: Option<Capture>,
    notice: Option<String>,
}

pub struct SettingsDialogResponse {
    pub open: bool,
    pub changed: bool,
}

impl SettingsDialog {
    pub fn new(section: Section) -> Self {
        Self {
            section,
            capture: None,
            notice: None,
        }
    }

    /// While waiting for a key, global shortcuts must be suspended.
    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn show(&mut self, ctx: &egui::Context, settings: &mut Settings) -> SettingsDialogResponse {
        let mut response = SettingsDialogResponse {
            open: true,
            changed: false,
        };
        response.changed |= self.capture_key(ctx, &mut settings.keymap);
        egui::Window::new(t!("settings.title"))
            .id(egui::Id::new("settings_window"))
            .open(&mut response.open)
            .collapsible(false)
            .default_size([640.0, 480.0])
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(170.0);
                        for section in Section::ALL {
                            ui.selectable_value(&mut self.section, section, section.title());
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| match self.section {
                        Section::General => {
                            response.changed |= general_section(ui, settings);
                        }
                        Section::Playback => {
                            response.changed |= playback_section(ui, settings);
                        }
                        Section::Shortcuts => {
                            response.changed |= self.shortcuts_section(ui, &mut settings.keymap);
                        }
                    });
                });
            });
        if !response.open {
            self.capture = None;
        }
        response
    }

    fn capture_key(&mut self, ctx: &egui::Context, keymap: &mut Keymap) -> bool {
        let Some(capture) = self.capture else {
            return false;
        };
        let pressed = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } => Some(Shortcut::from_event(*key, *modifiers)),
                egui::Event::Copy => Some(Shortcut::ctrl(egui::Key::C)),
                egui::Event::Cut => Some(Shortcut::ctrl(egui::Key::X)),
                egui::Event::Paste(_) => Some(Shortcut::ctrl(egui::Key::V)),
                _ => None,
            })
        });
        let Some(shortcut) = pressed else {
            return false;
        };
        self.capture = None;
        if shortcut == Shortcut::plain(egui::Key::Escape) {
            return false;
        }
        let stolen = keymap.assign(capture.action, capture.slot, shortcut);
        self.notice = (!stolen.is_empty()).then(|| {
            let names: Vec<Cow<str>> = stolen.iter().map(|a| a.label()).collect();
            t!(
                "settings.shortcut_taken_from",
                shortcut = shortcut,
                actions = names.join(", ")
            )
            .into_owned()
        });
        true
    }

    fn shortcuts_section(&mut self, ui: &mut egui::Ui, keymap: &mut Keymap) -> bool {
        let mut changed = false;
        ui.label(t!("settings.shortcuts_hint"));
        if let Some(notice) = &self.notice {
            ui.colored_label(egui::Color32::YELLOW, notice);
        }
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 36.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let mut category = Cow::Borrowed("");
                for action in Action::ALL {
                    if action.category() != category {
                        category = action.category();
                        ui.add_space(6.0);
                        ui.strong(category.as_ref());
                    }
                    ui.horizontal(|ui| {
                        ui.add_sized([220.0, 20.0], egui::Label::new(action.label()).truncate());
                        changed |= self.bindings_row(ui, keymap, action);
                    });
                }
            });
        ui.separator();
        if ui.button(t!("settings.restore_defaults")).clicked() {
            *keymap = Keymap::default();
            self.capture = None;
            self.notice = None;
            changed = true;
        }
        changed
    }

    fn bindings_row(&mut self, ui: &mut egui::Ui, keymap: &mut Keymap, action: Action) -> bool {
        let capture = self.capture;
        let waiting = |slot| capture.is_some_and(|c| c.action == action && c.slot == slot);
        let mut remove = None;
        for (slot, shortcut) in keymap.shortcuts(action).iter().enumerate() {
            let text = if waiting(Some(slot)) {
                t!("settings.press_a_key").into_owned()
            } else {
                shortcut.to_string()
            };
            if ui.button(text).clicked() {
                self.capture = Some(Capture {
                    action,
                    slot: Some(slot),
                });
            }
            if ui
                .small_button("x")
                .on_hover_text(t!("settings.remove"))
                .clicked()
            {
                remove = Some(slot);
            }
        }
        let add = if waiting(None) {
            t!("settings.press_a_key")
        } else {
            "+".into()
        };
        if ui
            .small_button(add)
            .on_hover_text(t!("settings.add_shortcut"))
            .clicked()
        {
            self.capture = Some(Capture { action, slot: None });
        }
        if let Some(slot) = remove {
            keymap.remove(action, slot);
            self.capture = None;
            return true;
        }
        false
    }
}

fn general_section(ui: &mut egui::Ui, settings: &mut Settings) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(t!("settings.language"));
        let items: Vec<_> = Language::ALL
            .iter()
            .map(|language| (*language, language.label().to_string(), true))
            .collect();
        changed |= preview_combo(
            ui,
            "settings_language",
            &mut settings.language,
            &items,
            None,
            None,
            None,
        );
    });
    if changed {
        settings.language.apply();
    }
    changed |= ui
        .checkbox(&mut settings.kinetic_scroll, t!("settings.kinetic_scroll"))
        .changed();
    changed
}

fn playback_section(ui: &mut egui::Ui, settings: &mut Settings) -> bool {
    let mut changed = false;
    ui.strong(t!("settings.read_ahead"));
    egui::Grid::new("settings_read_ahead")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label(t!("settings.read_ahead_forward"))
                .on_hover_text(t!("settings.read_ahead_forward_hint"));
            changed |= ui
                .add(
                    egui::DragValue::new(&mut settings.lookahead_secs)
                        .range(0.0..=30.0)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .on_hover_text(t!("settings.read_ahead_forward_hint"))
                .changed();
            ui.end_row();

            ui.label(t!("settings.read_ahead_behind"))
                .on_hover_text(t!("settings.read_ahead_behind_hint"));
            changed |= ui
                .add(
                    egui::DragValue::new(&mut settings.behind_secs)
                        .range(0.0..=30.0)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .on_hover_text(t!("settings.read_ahead_behind_hint"))
                .changed();
            ui.end_row();

            ui.label(t!("settings.video_cache"))
                .on_hover_text(t!("settings.video_cache_hint"));
            let mut budget_mb = (settings.cache_budget_bytes / 1_000_000) as u32;
            if ui
                .add(
                    egui::DragValue::new(&mut budget_mb)
                        .range(100..=8000)
                        .suffix(" MB"),
                )
                .on_hover_text(t!("settings.video_cache_hint"))
                .changed()
            {
                settings.cache_budget_bytes = budget_mb as usize * 1_000_000;
                changed = true;
            }
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.strong(t!("settings.proxy"));
    changed |= ui
        .checkbox(&mut settings.proxy_enabled, t!("menu.use_proxy"))
        .on_hover_text(t!("menu.use_proxy_hint"))
        .changed();
    ui.add_enabled_ui(settings.proxy_enabled, |ui| {
        ui.label(t!("settings.proxy_quality"));
        for quality in ProxyQuality::ALL {
            let (label, hint) = match quality {
                ProxyQuality::Low => (
                    t!("settings.proxy_quality_low"),
                    t!("settings.proxy_quality_low_hint"),
                ),
                ProxyQuality::Medium => (
                    t!("settings.proxy_quality_medium"),
                    t!("settings.proxy_quality_medium_hint"),
                ),
                ProxyQuality::High => (
                    t!("settings.proxy_quality_high"),
                    t!("settings.proxy_quality_high_hint"),
                ),
            };
            let text = format!("{label} ({} px)", quality.max_width());
            changed |= ui
                .radio_value(&mut settings.proxy_quality, quality, text)
                .on_hover_text(hint)
                .changed();
        }
        ui.label(egui::RichText::new(t!("settings.proxy_quality_note")).weak());
    });
    changed
}
