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
            rust_i18n::available_locales!().iter().any(|l| *l == lang).then_some(lang)
        })
        .unwrap_or_else(|| "en".to_owned())
}

#[cfg(test)]
mod tests {
    /// Every key passed to `t!` in the source must exist in all languages:
    /// a missing key would show up verbatim in the interface.
    #[test]
    fn every_key_used_in_the_source_is_translated_in_every_locale() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            for (at, _) in text.match_indices("t!(\"") {
                let before = text[..at].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let chunk = &text[at + 4..];
                let key = &chunk[..chunk.find('"').unwrap()];
                for locale in rust_i18n::available_locales!() {
                    if crate::_rust_i18n_try_translate(&locale, key).is_none() {
                        missing.push(format!("{locale}: {key} ({})", path.display()));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "traduzioni mancanti:\n{}", missing.join("\n"));
    }

    #[test]
    fn english_and_italian_are_available() {
        let locales = rust_i18n::available_locales!();
        assert!(["en", "it"].iter().all(|l| locales.iter().any(|x| x == l)), "{locales:?}");
    }
}
