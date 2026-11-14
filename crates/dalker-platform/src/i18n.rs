//! Languages: which ones Dalker speaks, which one this computer's user prefers, and the strings
//! of the host's own UI (tray, menu bar, message boxes).
//!
//! * The list of languages is `web-ui/i18n/languages.json`, shared with the web pages.
//! * The host UI's strings are `crates/dalker-platform/i18n/<code>.json`: flat objects of
//!   `"key": "text"`, with `{name}` placeholders. English (`en.json`) is the fallback for the
//!   language and for each missing string.
//! * The language is the one chosen in Dalker Settings (`DALKER_LANGUAGE` in the config file),
//!   or else the first supported one of the operating system's preferred languages, or else
//!   English ([`resolve`]). The web pages follow the same rules (`web-ui/i18n.js`).

use std::collections::HashMap;
use std::sync::OnceLock;

/// `DALKER_LANGUAGE` in the config file: a language code, or `auto` (or nothing) to follow the
/// operating system.
pub const CONFIG_KEY: &str = "DALKER_LANGUAGE";

const LANGUAGES_JSON: &str = include_str!("../../../web-ui/i18n/languages.json");

macro_rules! catalogs {
    ($($code:literal),* $(,)?) => {
        &[$(($code, include_str!(concat!("../i18n/", $code, ".json")))),*]
    };
}

/// The host UI's strings, by language code.
static CATALOGS: &[(&str, &str)] = catalogs!(
    "en", "uk", "pl", "de", "es", "fr", "pt-BR", "it", "ja", "ko", "zh-CN", "tr", "nl", "ar",
    "zh-TW", "vi", "id", "cs", "sv", "hi", "ro", "hu", "el", "th", "he", "da", "nb", "pt-PT", "fi",
    "sk", "bg", "hr", "sr", "lt", "lv", "et", "ms",
);

/// The supported language codes, in the order the pickers list them (English first).
pub fn languages() -> &'static [String] {
    static CODES: OnceLock<Vec<String>> = OnceLock::new();
    CODES.get_or_init(|| {
        let list: serde_json::Value =
            serde_json::from_str(LANGUAGES_JSON).expect("web-ui/i18n/languages.json is JSON");
        list.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l["code"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The code as listed, if `code` is a supported language (any letter case).
pub fn supported(code: &str) -> Option<&'static str> {
    languages()
        .iter()
        .find(|c| c.eq_ignore_ascii_case(code))
        .map(String::as_str)
}

/// The operating system's preferred languages, best first (BCP 47 tags such as `uk-UA`, or
/// POSIX locales on Linux).
pub fn system_languages() -> Vec<String> {
    sys_locale::get_locales().collect()
}

/// The supported language for one language tag: BCP 47 (`uk-UA`, `zh-Hant-TW`) or a POSIX
/// locale (`pt_BR.UTF-8`). Chinese goes by script or region, Portuguese by region (Brazil when
/// there is none), Norwegian Nynorsk and plain `no` to Bokmål.
pub fn match_tag(tag: &str) -> Option<&'static str> {
    let clean = tag
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    if clean.is_empty() || clean == "c" || clean == "posix" {
        return None;
    }
    let parts: Vec<&str> = clean.split('-').collect();
    let base = parts[0];
    let region = parts[1..].iter().copied().find(|p| {
        (p.len() == 2 && p.bytes().all(|b| b.is_ascii_alphabetic()))
            || (p.len() == 3 && p.bytes().all(|b| b.is_ascii_digit()))
    });
    let pick = match base {
        "zh" if parts.contains(&"hant") || matches!(region, Some("tw" | "hk" | "mo")) => {
            Some("zh-TW")
        }
        "zh" => Some("zh-CN"),
        "pt" if region.is_none_or(|r| r == "br") => Some("pt-BR"),
        "pt" => Some("pt-PT"),
        "no" | "nn" | "nb" => Some("nb"),
        "iw" => Some("he"),
        "in" => Some("id"),
        _ => None,
    };
    if let Some(found) = pick.and_then(supported) {
        return Some(found);
    }
    supported(&clean).or_else(|| {
        languages()
            .iter()
            .find(|c| {
                c.split('-')
                    .next()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(base)
            })
            .map(String::as_str)
    })
}

/// The language to use: `choice` when it is a supported code, else the first supported one of
/// `preferred`, else English.
pub fn resolve(choice: Option<&str>, preferred: &[String]) -> &'static str {
    choice
        .and_then(supported)
        .or_else(|| preferred.iter().find_map(|t| match_tag(t)))
        .unwrap_or("en")
}

/// [`resolve`] against this computer's preferred languages.
pub fn current(choice: Option<&str>) -> &'static str {
    resolve(choice, &system_languages())
}

type Catalog = HashMap<String, String>;

fn catalog(code: &str) -> Option<&'static Catalog> {
    static PARSED: OnceLock<HashMap<&'static str, Catalog>> = OnceLock::new();
    PARSED
        .get_or_init(|| {
            CATALOGS
                .iter()
                .map(|(code, text)| {
                    let strings = serde_json::from_str(text).unwrap_or_else(|e| {
                        tracing::warn!("host strings for {code} can't be read: {e}");
                        Catalog::new()
                    });
                    (*code, strings)
                })
                .collect()
        })
        .get(code)
}

/// The host UI string `key` in `lang` (English if it has none), with `{name}` placeholders
/// filled in from `args`.
pub fn tr(lang: &str, key: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let text = catalog(lang)
        .and_then(|c| c.get(key))
        .or_else(|| catalog("en").and_then(|c| c.get(key)))
        .map(String::as_str)
        .unwrap_or(key);
    let mut out = text.to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(text: &str) -> BTreeSet<String> {
        text.split('{')
            .skip(1)
            .filter_map(|p| p.split_once('}').map(|(name, _)| name.to_owned()))
            .collect()
    }

    #[test]
    fn every_language_has_host_strings() {
        let codes: Vec<&str> = CATALOGS.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            codes,
            languages(),
            "CATALOGS lists the languages.json codes"
        );
        assert_eq!(languages()[0], "en");
        let en = catalog("en").unwrap();
        assert!(!en.is_empty());
        for code in codes {
            let strings: Catalog =
                serde_json::from_str(CATALOGS.iter().find(|(c, _)| *c == code).unwrap().1)
                    .unwrap_or_else(|e| panic!("i18n/{code}.json: {e}"));
            let missing: Vec<&String> = en.keys().filter(|k| !strings.contains_key(*k)).collect();
            let extra: Vec<&String> = strings.keys().filter(|k| !en.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{code} lacks {missing:?}");
            assert!(extra.is_empty(), "{code} has unknown keys {extra:?}");
            for (key, text) in &strings {
                assert_eq!(
                    placeholders(text),
                    placeholders(&en[key]),
                    "{code} {key}: the same placeholders as in English"
                );
                assert!(!text.trim().is_empty(), "{code} {key} is empty");
            }
        }
    }

    #[test]
    fn matches_language_tags() {
        let m = |t: &str| match_tag(t);
        assert_eq!(m("uk-UA"), Some("uk"));
        assert_eq!(m("uk_UA.UTF-8"), Some("uk"));
        assert_eq!(m("pl"), Some("pl"));
        assert_eq!(m("en-GB"), Some("en"));
        assert_eq!(m("pt"), Some("pt-BR"));
        assert_eq!(m("pt-BR"), Some("pt-BR"));
        assert_eq!(m("pt_PT"), Some("pt-PT"));
        assert_eq!(m("pt-AO"), Some("pt-PT"));
        assert_eq!(m("zh"), Some("zh-CN"));
        assert_eq!(m("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(m("zh-SG"), Some("zh-CN"));
        assert_eq!(m("zh-Hant"), Some("zh-TW"));
        assert_eq!(m("zh-HK"), Some("zh-TW"));
        assert_eq!(m("zh_TW.UTF-8"), Some("zh-TW"));
        assert_eq!(m("no"), Some("nb"));
        assert_eq!(m("nn-NO"), Some("nb"));
        assert_eq!(m("sr-Latn-RS"), Some("sr"));
        assert_eq!(m("es-419"), Some("es"));
        assert_eq!(m("de-AT@euro"), Some("de"));
        assert_eq!(m("iw"), Some("he"));
        assert_eq!(m("C"), None);
        assert_eq!(m("POSIX"), None);
        assert_eq!(m("xx-YY"), None);
        assert_eq!(m(""), None);
    }

    #[test]
    fn resolves_the_language() {
        let prefs = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(resolve(None, &prefs(&["xx", "pl-PL", "uk"])), "pl");
        assert_eq!(resolve(Some("auto"), &prefs(&["uk-UA"])), "uk");
        assert_eq!(resolve(Some("DE"), &prefs(&["uk-UA"])), "de");
        assert_eq!(resolve(Some("xx"), &prefs(&["fr-CA"])), "fr");
        assert_eq!(resolve(None, &prefs(&["C"])), "en");
        assert_eq!(resolve(None, &[]), "en");
    }

    #[test]
    fn fills_in_strings() {
        assert_eq!(
            tr("en", "tray.viewers", &[("n", &3)]),
            "Viewers connected: 3"
        );
        assert_eq!(
            tr("xx", "tray.exit", &[]),
            "Exit Dalker",
            "English fallback"
        );
        assert_eq!(tr("en", "no.such.key", &[]), "no.such.key");
    }
}
