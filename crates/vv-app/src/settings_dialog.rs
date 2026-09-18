//! Finestra File → Impostazioni.

use crate::settings::{Action, Keymap, Shortcut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Shortcuts,
}

impl Section {
    const ALL: [Section; 1] = [Section::Shortcuts];

    fn title(self) -> &'static str {
        match self {
            Section::Shortcuts => "Scorciatoie da tastiera",
        }
    }
}

/// Scorciatoia in attesa del prossimo tasto: sostituisce la `slot`-esima,
/// o se ne aggiunge una se `None`.
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
    pub keymap_changed: bool,
}

impl SettingsDialog {
    pub fn new() -> Self {
        Self { section: Section::Shortcuts, capture: None, notice: None }
    }

    /// Finché aspetta un tasto le scorciatoie globali vanno sospese.
    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn show(&mut self, ctx: &egui::Context, keymap: &mut Keymap) -> SettingsDialogResponse {
        let mut response = SettingsDialogResponse { open: true, keymap_changed: false };
        response.keymap_changed |= self.capture_key(ctx, keymap);
        egui::Window::new("Impostazioni")
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
                        Section::Shortcuts => {
                            response.keymap_changed |= self.shortcuts_section(ui, keymap);
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
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => {
                    Some(Shortcut::from_event(*key, *modifiers))
                }
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
            let names: Vec<&str> = stolen.iter().map(|a| a.label()).collect();
            format!("{shortcut} tolta da: {}", names.join(", "))
        });
        true
    }

    fn shortcuts_section(&mut self, ui: &mut egui::Ui, keymap: &mut Keymap) -> bool {
        let mut changed = false;
        ui.label("Clicca una scorciatoia e premi i tasti nuovi (Esc annulla).");
        if let Some(notice) = &self.notice {
            ui.colored_label(egui::Color32::YELLOW, notice);
        }
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 36.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let mut category = "";
                for action in Action::ALL {
                    if action.category() != category {
                        category = action.category();
                        ui.add_space(6.0);
                        ui.strong(category);
                    }
                    ui.horizontal(|ui| {
                        ui.add_sized([220.0, 20.0], egui::Label::new(action.label()).truncate());
                        changed |= self.bindings_row(ui, keymap, action);
                    });
                }
            });
        ui.separator();
        if ui.button("Ripristina predefinite").clicked() {
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
                "Premi un tasto...".to_owned()
            } else {
                shortcut.to_string()
            };
            if ui.button(text).clicked() {
                self.capture = Some(Capture { action, slot: Some(slot) });
            }
            if ui.small_button("x").on_hover_text("Rimuovi").clicked() {
                remove = Some(slot);
            }
        }
        let add = if waiting(None) { "Premi un tasto..." } else { "+" };
        if ui.small_button(add).on_hover_text("Aggiungi scorciatoia").clicked() {
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
