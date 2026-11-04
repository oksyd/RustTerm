use super::{LOCALES, messages, tr};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use unic_langid::LanguageIdentifier;

/// A UI language registered by the embedded locale manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Language(Option<&'static str>);

#[allow(non_upper_case_globals)]
impl Language {
    pub const System: Self = Self(None);
    pub const English: Self = Self(Some("en"));
    pub const SimplifiedChinese: Self = Self(Some("zh-CN"));

    pub const fn code(self) -> &'static str {
        match self.0 {
            Some(code) => code,
            None => "system",
        }
    }

    pub fn native_name(self) -> &'static str {
        match self.0 {
            Some(_) => LOCALES[self.locale_index()].native_name,
            None => tr(messages::COMMON_FOLLOW_SYSTEM),
        }
    }

    /// Settings and locale selection share the same generated registry.
    pub fn available() -> impl Iterator<Item = Self> {
        std::iter::once(Self::System).chain(LOCALES.iter().map(|locale| Self(Some(locale.code))))
    }

    pub(super) fn locale_index(self) -> usize {
        let language = if self == Self::System {
            system_language()
        } else {
            self
        };
        LOCALES
            .iter()
            .position(|locale| locale.code == language.code())
            .expect("language belongs to the embedded locale registry")
    }
}

impl Serialize for Language {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Self::available()
            .find(|language| language.code() == code)
            .ok_or_else(|| de::Error::custom(format!("unsupported UI language: {code}")))
    }
}

static LANGUAGE: AtomicUsize = AtomicUsize::new(0);

/// Switch UI language without changing process locale or terminal data. The
/// caller persists this preference and invalidates its views.
pub fn set_language(language: Language) {
    let index = if language == Language::System {
        0
    } else {
        language.locale_index() + 1
    };
    LANGUAGE.store(index, Ordering::Relaxed);
}

/// Return the resolved language, including when the preference follows Linux.
pub fn current_language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        0 => system_language(),
        index => Language(Some(LOCALES[index - 1].code)),
    }
}

fn system_language() -> Language {
    static SYSTEM: OnceLock<Language> = OnceLock::new();
    *SYSTEM.get_or_init(|| {
        language_from_environment(
            std::env::var("LC_ALL").ok().as_deref(),
            std::env::var("LC_MESSAGES").ok().as_deref(),
            std::env::var("LANG").ok().as_deref(),
            std::env::var("LANGUAGE").ok().as_deref(),
        )
    })
}

fn language_from_environment(
    all: Option<&str>,
    messages: Option<&str>,
    lang: Option<&str>,
    preferences: Option<&str>,
) -> Language {
    let locale = [all, messages, lang]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .unwrap_or("C");
    // GNU LANGUAGE does not override the C/POSIX locale, including C.UTF-8.
    let base = locale.split(['.', '@']).next().unwrap_or(locale);
    if base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return Language::English;
    }
    if let Some(preferences) = preferences {
        for preference in preferences.split(':') {
            if let Some(language) = locale_language(preference) {
                return language;
            }
        }
    }
    locale_language(locale).unwrap_or(Language::English)
}

fn locale_language(locale: &str) -> Option<Language> {
    let requested: LanguageIdentifier = locale.split(['.', '@']).next()?.parse().ok()?;
    let mut expanded = requested.clone();
    expanded.maximize();
    LOCALES
        .iter()
        .filter_map(|locale| {
            let candidate: LanguageIdentifier = locale.code.parse().expect("validated locale");
            let mut candidate_expanded = candidate.clone();
            candidate_expanded.maximize();
            if expanded.language != candidate_expanded.language
                || expanded.script != candidate_expanded.script
            {
                return None;
            }
            // Exact tags win; regional variants remain within the same script.
            let rank = if requested == candidate {
                0
            } else if expanded.region == candidate_expanded.region {
                1
            } else {
                2
            };
            Some((rank, Language(Some(locale.code))))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, language)| language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_locale_precedence_and_fallback() {
        for (all, messages, lang, preferences, expected) in [
            (
                None,
                None,
                Some("zh_CN.UTF-8"),
                None,
                Language::SimplifiedChinese,
            ),
            (None, Some("en_US"), Some("zh_CN"), None, Language::English),
            (
                Some("C"),
                Some("zh_CN"),
                None,
                Some("zh_CN"),
                Language::English,
            ),
            (
                Some("C.UTF-8"),
                None,
                None,
                Some("zh_CN"),
                Language::English,
            ),
            (Some("POSIX"), None, None, Some("zh_CN"), Language::English),
            (
                Some(""),
                None,
                Some("en_US.UTF-8"),
                Some("zz:zh-CN:en"),
                Language::SimplifiedChinese,
            ),
            (None, None, Some("zz_ZZ.UTF-8"), None, Language::English),
            (None, None, None, None, Language::English),
        ] {
            assert_eq!(
                language_from_environment(all, messages, lang, preferences),
                expected
            );
        }
    }

    #[test]
    fn locale_matching_preserves_script_and_accepts_regional_variants() {
        for locale in ["zh", "zh-CN", "zh_SG.UTF-8", "zh-Hans"] {
            assert_eq!(locale_language(locale), Some(Language::SimplifiedChinese));
        }
        for locale in ["zh-TW", "zh-HK", "zh-Hant"] {
            assert_ne!(locale_language(locale), Some(Language::SimplifiedChinese));
        }
        assert_eq!(locale_language("en_GB.UTF-8"), Some(Language::English));
        assert_eq!(locale_language("not a locale"), None);
    }

    #[test]
    fn registered_languages_round_trip_existing_configuration_codes() {
        assert_eq!(Language::default(), Language::System);
        for language in Language::available() {
            let encoded = serde_json::to_string(&language).unwrap();
            assert_eq!(
                serde_json::from_str::<Language>(&encoded).unwrap(),
                language
            );
        }
        assert!(serde_json::from_str::<Language>("\"unregistered\"").is_err());
    }
}
