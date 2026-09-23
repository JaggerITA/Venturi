/// Every key passed to `t!` in the source must exist in all languages:
/// a missing key would show up verbatim in the interface.
#[test]
fn every_key_used_in_the_source_is_translated_in_every_locale() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
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
    assert!(missing.is_empty(), "missing translations:\n{}", missing.join("\n"));
}

#[test]
fn english_and_italian_are_available() {
    let locales = rust_i18n::available_locales!();
    assert!(["en", "it"].iter().all(|l| locales.iter().any(|x| x == l)), "{locales:?}");
}
