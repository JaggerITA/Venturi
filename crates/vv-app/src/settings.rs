//! Impostazioni utente del programma (non del progetto), salvate in
//! `~/.config/vibevideo/settings.json`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::i18n::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    TogglePlayback,
    FastPlayback,
    StepBackward,
    StepForward,
    MarkIn,
    MarkOut,
    FullscreenViewer,
    Undo,
    Redo,
    Copy,
    Cut,
    Paste,
    Delete,
    RippleDelete,
    Split,
    ToggleDisabled,
    SelectAll,
    SelectFromPlayhead,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ImportMedia,
    Export,
    ZoomIn,
    ZoomOut,
}

impl Action {
    pub const ALL: [Action; 25] = [
        Action::TogglePlayback,
        Action::FastPlayback,
        Action::StepBackward,
        Action::StepForward,
        Action::MarkIn,
        Action::MarkOut,
        Action::FullscreenViewer,
        Action::Undo,
        Action::Redo,
        Action::Copy,
        Action::Cut,
        Action::Paste,
        Action::Delete,
        Action::RippleDelete,
        Action::Split,
        Action::ToggleDisabled,
        Action::SelectAll,
        Action::SelectFromPlayhead,
        Action::OpenProject,
        Action::SaveProject,
        Action::SaveProjectAs,
        Action::ImportMedia,
        Action::Export,
        Action::ZoomIn,
        Action::ZoomOut,
    ];

    /// Chiave nel file di impostazioni: non va mai cambiata.
    pub fn id(self) -> &'static str {
        match self {
            Action::TogglePlayback => "toggle_playback",
            Action::FastPlayback => "fast_playback",
            Action::StepBackward => "step_backward",
            Action::StepForward => "step_forward",
            Action::MarkIn => "mark_in",
            Action::MarkOut => "mark_out",
            Action::FullscreenViewer => "fullscreen_viewer",
            Action::Undo => "undo",
            Action::Redo => "redo",
            Action::Copy => "copy",
            Action::Cut => "cut",
            Action::Paste => "paste",
            Action::Delete => "delete",
            Action::RippleDelete => "ripple_delete",
            Action::Split => "split",
            Action::ToggleDisabled => "toggle_disabled",
            Action::SelectAll => "select_all",
            Action::SelectFromPlayhead => "select_from_playhead",
            Action::OpenProject => "open_project",
            Action::SaveProject => "save_project",
            Action::SaveProjectAs => "save_project_as",
            Action::ImportMedia => "import_media",
            Action::Export => "export",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
        }
    }

    pub fn label(self) -> Cow<'static, str> {
        t!(format!("action.{}", self.id()))
    }

    pub fn category(self) -> Cow<'static, str> {
        match self {
            Action::TogglePlayback
            | Action::FastPlayback
            | Action::StepBackward
            | Action::StepForward
            | Action::MarkIn
            | Action::MarkOut
            | Action::FullscreenViewer => t!("action_category.playback"),
            Action::Undo
            | Action::Redo
            | Action::Copy
            | Action::Cut
            | Action::Paste
            | Action::Delete
            | Action::RippleDelete
            | Action::Split
            | Action::ToggleDisabled
            | Action::SelectAll
            | Action::SelectFromPlayhead => t!("action_category.edit"),
            Action::OpenProject
            | Action::SaveProject
            | Action::SaveProjectAs
            | Action::ImportMedia
            | Action::Export => t!("action_category.file"),
            Action::ZoomIn | Action::ZoomOut => t!("action_category.timeline"),
        }
    }

    fn default_shortcuts(self) -> Vec<Shortcut> {
        use egui::Key;
        let plain = Shortcut::plain;
        let ctrl = Shortcut::ctrl;
        let ctrl_shift = |key| Shortcut { shift: true, ..Shortcut::ctrl(key) };
        match self {
            Action::TogglePlayback => vec![plain(Key::Space)],
            Action::FastPlayback => vec![plain(Key::A)],
            Action::StepBackward => vec![plain(Key::ArrowLeft)],
            Action::StepForward => vec![plain(Key::ArrowRight)],
            Action::MarkIn => vec![plain(Key::I)],
            Action::MarkOut => vec![plain(Key::O)],
            Action::FullscreenViewer => vec![ctrl(Key::F)],
            Action::Undo => vec![ctrl(Key::Z)],
            Action::Redo => vec![ctrl_shift(Key::Z)],
            Action::Copy => vec![ctrl(Key::C)],
            Action::Cut => vec![ctrl(Key::X)],
            Action::Paste => vec![ctrl(Key::V)],
            Action::Delete => vec![plain(Key::Delete), plain(Key::Backspace)],
            // Tasto ISO tra Shift sinistro e Z ("<" sui layout italiani).
            Action::RippleDelete => vec![plain(Key::IntlBackslash)],
            Action::Split => vec![plain(Key::T)],
            Action::ToggleDisabled => vec![plain(Key::D)],
            Action::SelectAll => vec![ctrl(Key::A)],
            Action::SelectFromPlayhead => vec![Shortcut { alt: true, ..plain(Key::Y) }],
            Action::OpenProject => vec![ctrl(Key::O)],
            Action::SaveProject => vec![ctrl(Key::S)],
            Action::SaveProjectAs => vec![ctrl_shift(Key::S)],
            Action::ImportMedia => vec![ctrl(Key::I)],
            Action::Export => vec![ctrl_shift(Key::E)],
            // "=" è il "+" non shiftato dei layout US.
            Action::ZoomIn => vec![ctrl(Key::Plus), ctrl(Key::Equals)],
            Action::ZoomOut => vec![ctrl(Key::Minus)],
        }
    }

    fn from_id(id: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.id() == id)
    }
}

/// `ctrl` è Cmd su macOS (`Modifiers::command`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub key: egui::Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Shortcut {
    pub fn plain(key: egui::Key) -> Self {
        Self { key, ctrl: false, shift: false, alt: false }
    }

    pub fn ctrl(key: egui::Key) -> Self {
        Self { ctrl: true, ..Self::plain(key) }
    }

    pub fn from_event(key: egui::Key, modifiers: egui::Modifiers) -> Self {
        Self { key, ctrl: modifiers.command, shift: modifiers.shift, alt: modifiers.alt }
    }

    fn modifiers(self) -> egui::Modifiers {
        egui::Modifiers { alt: self.alt, shift: self.shift, command: self.ctrl, ..Default::default() }
    }

    fn modifiers_match(self, current: egui::Modifiers) -> bool {
        // Sui simboli Shift può servire solo a produrre il tasto ("+" sui
        // layout US): lì si ignora se la scorciatoia non lo chiede.
        if is_symbol(self.key) {
            current.matches_logically(self.modifiers()) && (self.alt || !current.alt)
        } else {
            current.matches_exact(self.modifiers())
        }
    }

    /// Ctrl+C/X/V arrivano da eframe come eventi `Copy`/`Cut`/`Paste`,
    /// non come pressioni di tasto.
    fn clipboard_event(self) -> Option<fn(&egui::Event) -> bool> {
        if !self.ctrl || self.shift || self.alt {
            return None;
        }
        match self.key {
            egui::Key::C => Some(|e| matches!(e, egui::Event::Copy)),
            egui::Key::X => Some(|e| matches!(e, egui::Event::Cut)),
            egui::Key::V => Some(|e| matches!(e, egui::Event::Paste(_))),
            _ => None,
        }
    }

    pub fn pressed(self, input: &egui::InputState) -> bool {
        if let Some(is_event) = self.clipboard_event()
            && input.events.iter().any(is_event)
        {
            return true;
        }
        input.key_pressed(self.key) && self.modifiers_match(input.modifiers)
    }

    pub fn down(self, input: &egui::InputState) -> bool {
        input.key_down(self.key) && self.modifiers_match(input.modifiers)
    }

    /// Formato del file di impostazioni, es. `Ctrl+Shift+S`.
    fn to_config(self) -> String {
        self.join(self.key.name())
    }

    fn from_config(text: &str) -> Option<Self> {
        let mut parts: Vec<&str> = text.split('+').collect();
        // "Ctrl++": l'ultimo "+" è il tasto, non un separatore.
        let key_name = if text.ends_with("++") {
            parts.truncate(parts.len() - 2);
            "+"
        } else {
            parts.pop()?
        };
        let mut shortcut = Self::plain(egui::Key::from_name(key_name)?);
        for part in parts {
            match part {
                "Ctrl" => shortcut.ctrl = true,
                "Shift" => shortcut.shift = true,
                "Alt" => shortcut.alt = true,
                _ => return None,
            }
        }
        Some(shortcut)
    }

    fn join(self, key: &str) -> String {
        let mut text = String::new();
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                text.push_str(name);
            }
        }
        text.push_str(key);
        text
    }
}

impl std::fmt::Display for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key: Cow<str> = match self.key {
            egui::Key::IntlBackslash => "<".into(),
            egui::Key::Minus => "-".into(),
            egui::Key::ArrowLeft => "←".into(),
            egui::Key::ArrowRight => "→".into(),
            egui::Key::ArrowUp => "↑".into(),
            egui::Key::ArrowDown => "↓".into(),
            egui::Key::Space => t!("key.space"),
            egui::Key::Delete => t!("key.delete"),
            key => key.symbol_or_name().into(),
        };
        f.write_str(&self.join(&key))
    }
}

fn is_symbol(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(
        key,
        Plus | Minus
            | Equals
            | Colon
            | Semicolon
            | Comma
            | Period
            | Slash
            | Backslash
            | Pipe
            | Questionmark
            | Exclamationmark
            | Quote
            | Backtick
            | OpenBracket
            | CloseBracket
            | OpenCurlyBracket
            | CloseCurlyBracket
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<Action, Vec<Shortcut>>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            bindings: Action::ALL.into_iter().map(|a| (a, a.default_shortcuts())).collect(),
        }
    }
}

impl Keymap {
    pub fn shortcuts(&self, action: Action) -> &[Shortcut] {
        self.bindings.get(&action).map_or(&[], Vec::as_slice)
    }

    pub fn pressed(&self, action: Action, input: &egui::InputState) -> bool {
        self.shortcuts(action).iter().any(|s| s.pressed(input))
    }

    pub fn down(&self, action: Action, input: &egui::InputState) -> bool {
        self.shortcuts(action).iter().any(|s| s.down(input))
    }

    /// `"testo (scorciatoia)"`, o solo `"testo"` se l'azione non ne ha.
    pub fn menu_label(&self, text: &str, action: Action) -> String {
        match self.shortcuts(action).first() {
            Some(shortcut) => format!("{text} ({shortcut})"),
            None => text.to_owned(),
        }
    }

    /// Assegna `shortcut` ad `action` (al posto di `slot`, o in aggiunta se
    /// `None`), togliendola da qualunque altra azione. Restituisce le
    /// azioni a cui è stata tolta.
    pub fn assign(&mut self, action: Action, slot: Option<usize>, shortcut: Shortcut) -> Vec<Action> {
        let mut stolen = Vec::new();
        for (&other, shortcuts) in &mut self.bindings {
            if other != action && shortcuts.contains(&shortcut) {
                shortcuts.retain(|s| *s != shortcut);
                stolen.push(other);
            }
        }
        let shortcuts = self.bindings.entry(action).or_default();
        match slot.filter(|&i| i < shortcuts.len()) {
            Some(i) => shortcuts[i] = shortcut,
            None => shortcuts.push(shortcut),
        }
        let mut seen = Vec::new();
        shortcuts.retain(|s| {
            let first = !seen.contains(s);
            seen.push(*s);
            first
        });
        stolen
    }

    pub fn remove(&mut self, action: Action, slot: usize) {
        if let Some(shortcuts) = self.bindings.get_mut(&action)
            && slot < shortcuts.len()
        {
            shortcuts.remove(slot);
        }
    }
}

const MAX_RECENT_PROJECTS: usize = 10;

/// Stato dei pannelli della UI (dimensione, aperto/chiuso): salvato per
/// ritrovare l'interfaccia com'è stata lasciata alla riapertura.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelLayout {
    pub media_pool_open: bool,
    pub effects_open: bool,
    pub inspector_open: bool,
    pub keyframe_editor_open: bool,
    pub left_column_width: f32,
    pub inspector_width: f32,
    pub timeline_height: f32,
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            media_pool_open: true,
            effects_open: false,
            inspector_open: true,
            keyframe_editor_open: false,
            left_column_width: 260.0,
            inspector_width: 300.0,
            timeline_height: 240.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub keymap: Keymap,
    pub language: Language,
    /// Scrolling cinetico (inerzia dopo lo swipe da touchpad) sulla timeline.
    pub kinetic_scroll: bool,
    /// Anteprima dal proxy tutto-intra quando pronto: scrub fluido su sorgenti
    /// long-GOP. L'export usa sempre i sorgenti.
    pub proxy_enabled: bool,
    /// Progetti aperti di recente, più recente per primo.
    pub recent_projects: Vec<PathBuf>,
    pub panels: PanelLayout,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keymap: Keymap::default(),
            language: Language::default(),
            kinetic_scroll: true,
            proxy_enabled: true,
            recent_projects: Vec::new(),
            panels: PanelLayout::default(),
        }
    }
}

impl Settings {
    pub fn add_recent_project(&mut self, path: PathBuf) {
        self.recent_projects.retain(|p| p != &path);
        self.recent_projects.insert(0, path);
        self.recent_projects.truncate(MAX_RECENT_PROJECTS);
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SettingsFile {
    #[serde(default)]
    shortcuts: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    kinetic_scroll: Option<bool>,
    #[serde(default)]
    proxy_enabled: Option<bool>,
    #[serde(default)]
    recent_projects: Vec<PathBuf>,
    #[serde(default)]
    panels: PanelLayoutFile,
}

#[derive(Serialize, Deserialize, Default)]
struct PanelLayoutFile {
    #[serde(default)]
    media_pool_open: Option<bool>,
    #[serde(default)]
    effects_open: Option<bool>,
    #[serde(default)]
    inspector_open: Option<bool>,
    #[serde(default)]
    keyframe_editor_open: Option<bool>,
    #[serde(default)]
    left_column_width: Option<f32>,
    #[serde(default)]
    inspector_width: Option<f32>,
    #[serde(default)]
    timeline_height: Option<f32>,
}

impl Settings {
    pub fn default_path() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("vibevideo").join("settings.json"))
    }

    /// File assente o illeggibile = impostazioni predefinite; le azioni
    /// assenti dal file tengono le scorciatoie predefinite.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let file: SettingsFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(e) => {
                eprintln!("invalid settings in {}: {e}", path.display());
                return Self::default();
            }
        };
        let mut settings = Self::default();
        settings.language = file.language.as_deref().and_then(Language::from_id).unwrap_or_default();
        settings.kinetic_scroll = file.kinetic_scroll.unwrap_or(true);
        settings.proxy_enabled = file.proxy_enabled.unwrap_or(true);
        settings.recent_projects = file.recent_projects;
        let defaults = PanelLayout::default();
        settings.panels = PanelLayout {
            media_pool_open: file.panels.media_pool_open.unwrap_or(defaults.media_pool_open),
            effects_open: file.panels.effects_open.unwrap_or(defaults.effects_open),
            inspector_open: file.panels.inspector_open.unwrap_or(defaults.inspector_open),
            keyframe_editor_open: file
                .panels
                .keyframe_editor_open
                .unwrap_or(defaults.keyframe_editor_open),
            left_column_width: file.panels.left_column_width.unwrap_or(defaults.left_column_width),
            inspector_width: file.panels.inspector_width.unwrap_or(defaults.inspector_width),
            timeline_height: file.panels.timeline_height.unwrap_or(defaults.timeline_height),
        };
        for (id, shortcuts) in file.shortcuts {
            let Some(action) = Action::from_id(&id) else {
                continue;
            };
            let parsed = shortcuts.iter().filter_map(|s| Shortcut::from_config(s)).collect();
            settings.keymap.bindings.insert(action, parsed);
        }
        settings
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let file = SettingsFile {
            shortcuts: self
                .keymap
                .bindings
                .iter()
                .map(|(action, shortcuts)| {
                    (action.id().to_owned(), shortcuts.iter().map(|s| s.to_config()).collect())
                })
                .collect(),
            language: Some(self.language.id().to_owned()),
            kinetic_scroll: Some(self.kinetic_scroll),
            proxy_enabled: Some(self.proxy_enabled),
            recent_projects: self.recent_projects.clone(),
            panels: PanelLayoutFile {
                media_pool_open: Some(self.panels.media_pool_open),
                effects_open: Some(self.panels.effects_open),
                inspector_open: Some(self.panels.inspector_open),
                keyframe_editor_open: Some(self.panels.keyframe_editor_open),
                left_column_width: Some(self.panels.left_column_width),
                inspector_width: Some(self.panels.inspector_width),
                timeline_height: Some(self.panels.timeline_height),
            },
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_round_trip_through_the_config_format() {
        for action in Action::ALL {
            for shortcut in action.default_shortcuts() {
                assert_eq!(Shortcut::from_config(&shortcut.to_config()), Some(shortcut));
            }
        }
        assert_eq!(Shortcut::from_config("Ctrl++"), Some(Shortcut::ctrl(egui::Key::Plus)));
    }

    #[test]
    fn every_action_label_is_translated() {
        for locale in rust_i18n::available_locales!() {
            for action in Action::ALL {
                let key = format!("action.{}", action.id());
                assert!(crate::_rust_i18n_try_translate(&locale, &key).is_some(), "{locale}: {key}");
            }
        }
    }

    #[test]
    fn default_shortcuts_are_unique() {
        let keymap = Keymap::default();
        let mut seen = Vec::new();
        for action in Action::ALL {
            for shortcut in keymap.shortcuts(action) {
                assert!(!seen.contains(shortcut), "{shortcut} usata due volte");
                seen.push(*shortcut);
            }
        }
    }

    #[test]
    fn assigning_a_shortcut_takes_it_away_from_other_actions() {
        let mut keymap = Keymap::default();
        let stolen = keymap.assign(Action::Split, Some(0), Shortcut::plain(egui::Key::D));
        assert_eq!(stolen, vec![Action::ToggleDisabled]);
        assert_eq!(keymap.shortcuts(Action::Split), &[Shortcut::plain(egui::Key::D)]);
        assert!(keymap.shortcuts(Action::ToggleDisabled).is_empty());
    }

    #[test]
    fn save_then_load_keeps_custom_shortcuts_and_defaults_for_the_rest() {
        let path = std::env::temp_dir()
            .join(format!("vv-settings-{}", std::process::id()))
            .join("settings.json");
        let mut settings = Settings::default();
        settings.keymap.assign(Action::Split, Some(0), Shortcut::ctrl(egui::Key::K));
        settings.keymap.remove(Action::ZoomIn, 1);
        settings.language = Language::Italian;
        settings.proxy_enabled = false;
        settings.save(&path).unwrap();

        let loaded = Settings::load(&path);
        assert_eq!(loaded, settings);

        std::fs::write(&path, r#"{"shortcuts": {"split": ["Alt+K"]}}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.keymap.shortcuts(Action::Split), &[Shortcut { alt: true, ..Shortcut::plain(egui::Key::K) }]);
        assert_eq!(loaded.keymap.shortcuts(Action::Undo), Keymap::default().shortcuts(Action::Undo));
        assert!(loaded.proxy_enabled);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_then_load_keeps_custom_panel_layout_and_defaults_it_when_absent() {
        let path = std::env::temp_dir()
            .join(format!("vv-settings-panels-{}", std::process::id()))
            .join("settings.json");
        let mut settings = Settings::default();
        settings.panels = PanelLayout {
            media_pool_open: false,
            effects_open: true,
            inspector_open: false,
            keyframe_editor_open: true,
            left_column_width: 321.0,
            inspector_width: 456.0,
            timeline_height: 199.0,
        };
        settings.save(&path).unwrap();

        let loaded = Settings::load(&path);
        assert_eq!(loaded, settings);

        std::fs::write(&path, "{}").unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.panels, PanelLayout::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn shift_is_ignored_on_symbols_but_not_on_letters() {
        let ctx = egui::Context::default();
        let press = |key, modifiers| {
            let mut input = egui::RawInput::default();
            input.events.push(egui::Event::ModifiersChanged(modifiers));
            input.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            let mut result = (false, false);
            let mut output = ctx.run_ui(input, |ui| {
                ui.input(|i| {
                    result = (
                        Shortcut::ctrl(egui::Key::Plus).pressed(i),
                        Shortcut::ctrl(egui::Key::S).pressed(i),
                    )
                });
            });
            output.textures_delta.clear();
            result
        };
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        assert_eq!(press(egui::Key::Plus, ctrl_shift), (true, false));
        assert_eq!(press(egui::Key::S, ctrl_shift), (false, false));
        assert_eq!(press(egui::Key::S, egui::Modifiers::COMMAND), (false, true));
    }
}
