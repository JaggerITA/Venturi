//! Interface language; the texts live in `locales/*.yml`.

use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    System,
    English,
    Italian,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::System, Language::English, Language::Italian];

    /// Key in the settings file: must never be changed.
    pub fn id(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::English => "en",
            Language::Italian => "it",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.id() == id)
    }

    /// Language names stay in the language itself, so it can be found again
    /// even from an interface one cannot read.
    pub fn label(self) -> Cow<'static, str> {
        match self {
            Language::System => t!("settings.language_system"),
            Language::English => "English".into(),
            Language::Italian => "Italiano".into(),
        }
    }

    pub fn apply(self) {
        let locale = match self {
            Language::System => system_locale(),
            other => other.id().to_owned(),
        };
        rust_i18n::set_locale(&locale);
    }
}

fn system_locale() -> String {
    sys_locale::get_locale()
        .and_then(|locale| {
            let lang = locale.split(['-', '_']).next()?.to_lowercase();
            rust_i18n::available_locales!()
                .iter()
                .any(|l| *l == lang)
                .then_some(lang)
        })
        .unwrap_or_else(|| "en".to_owned())
}

#[cfg(test)]
#[path = "tests/i18n.rs"]
mod tests;
