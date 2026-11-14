//! Checks the web client's translations (`web-ui/i18n/`, see `web-ui/i18n.js`): every listed
//! language has a file, every file has the English keys and no others, with the same
//! placeholders and plural forms, and every key the pages use exists.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::Assets;

fn json(path: &str) -> Value {
    let file = Assets::get(path).unwrap_or_else(|| panic!("web-ui/{path} is missing"));
    serde_json::from_slice(&file.data).unwrap_or_else(|e| panic!("web-ui/{path}: {e}"))
}

fn text(path: &str) -> String {
    String::from_utf8(Assets::get(path).unwrap().data.into_owned()).unwrap()
}

fn strings(code: &str) -> Map<String, Value> {
    match json(&format!("i18n/{code}.json")) {
        Value::Object(m) => m,
        _ => panic!("i18n/{code}.json is not an object"),
    }
}

fn placeholders(text: &str) -> BTreeSet<String> {
    text.split('{')
        .skip(1)
        .filter_map(|p| p.split_once('}').map(|(name, _)| name.to_owned()))
        .collect()
}

/// The plural forms each language needs (Intl.PluralRules categories, as used for whole
/// numbers).
fn plural_forms(code: &str) -> &'static [&'static str] {
    match code.split('-').next().unwrap() {
        "ja" | "ko" | "zh" | "vi" | "id" | "th" | "ms" | "tr" => &["other"],
        "uk" | "sr" | "hr" => &["one", "few", "many", "other"],
        "pl" | "lt" => &["one", "few", "many", "other"],
        "cs" | "sk" => &["one", "few", "many", "other"],
        "ar" => &["zero", "one", "two", "few", "many", "other"],
        "he" => &["one", "two", "other"],
        "ro" => &["one", "few", "other"],
        "lv" => &["zero", "one", "other"],
        "fr" | "pt" | "es" | "it" => &["one", "many", "other"],
        _ => &["one", "other"],
    }
}

fn codes() -> Vec<String> {
    json("i18n/languages.json")
        .as_array()
        .expect("languages.json is a list")
        .iter()
        .map(|l| l["code"].as_str().expect("a code").to_owned())
        .collect()
}

#[test]
fn every_language_has_every_string() {
    let codes = codes();
    assert_eq!(codes[0], "en");
    let en = strings("en");
    for code in &codes {
        let tr = strings(code);
        let missing: Vec<&String> = en.keys().filter(|k| !tr.contains_key(*k)).collect();
        let extra: Vec<&String> = tr.keys().filter(|k| !en.contains_key(*k)).collect();
        assert!(missing.is_empty(), "{code} lacks {missing:?}");
        assert!(extra.is_empty(), "{code} has unknown keys {extra:?}");
        let mut same = 0;
        for (key, english) in &en {
            let value = &tr[key];
            match english {
                Value::String(e) => {
                    let t = value
                        .as_str()
                        .unwrap_or_else(|| panic!("{code} {key}: a string"));
                    assert!(!t.trim().is_empty(), "{code} {key} is empty");
                    assert_eq!(placeholders(t), placeholders(e), "{code} {key}");
                    same += usize::from(t == e);
                }
                Value::Object(forms) => {
                    let wanted = placeholders(forms["other"].as_str().unwrap());
                    let got = value
                        .as_object()
                        .unwrap_or_else(|| panic!("{code} {key}: plural forms"));
                    let needed: &[&str] = if code == "en" {
                        &["one", "other"]
                    } else {
                        plural_forms(code)
                    };
                    for form in needed {
                        let t = got
                            .get(*form)
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| panic!("{code} {key} lacks the {form:?} form"));
                        assert_eq!(placeholders(t), wanted, "{code} {key} {form}");
                    }
                }
                _ => panic!("en {key}: a string or plural forms"),
            }
        }
        // A file that is mostly English was copied, not translated.
        if code != "en" {
            assert!(
                same * 4 < en.len(),
                "{code}: {same} strings are still English"
            );
        }
    }
}

/// Every key the pages use (data-i18n attributes, I18N.t / tr calls with a literal key) is in
/// en.json.
#[test]
fn pages_use_known_keys() {
    let en = strings("en");
    let mut used = BTreeSet::new();
    for page in ["index.html", "settings.html"] {
        let html = text(page);
        for attr in [
            "data-i18n=\"",
            "data-i18n-title=\"",
            "data-i18n-aria-label=\"",
            "data-i18n-placeholder=\"",
        ] {
            for part in html.split(attr).skip(1) {
                used.insert(part.split('"').next().unwrap().to_owned());
            }
        }
    }
    for script in ["app.js", "settings.js"] {
        let js = text(script);
        for quote in ['"', '`'] {
            for call in ["tr(", "I18N.t("] {
                let open = format!("{call}{quote}");
                for part in js.split(open.as_str()).skip(1) {
                    let key = part.split(quote).next().unwrap();
                    // A key built from parts (`settings.${kind}.moves`) is checked by its uses.
                    if !key.contains("${") {
                        used.insert(key.to_owned());
                    }
                }
            }
        }
    }
    assert!(used.len() > 100, "found the keys: {}", used.len());
    let unknown: Vec<&String> = used.iter().filter(|k| !en.contains_key(*k)).collect();
    assert!(unknown.is_empty(), "keys missing from en.json: {unknown:?}");
}
