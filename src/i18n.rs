//! Embedded Fluent catalogs for application text. Terminal output, filenames and
//! user input are values, never translation identifiers.

mod language;
mod message;

use fluent_bundle::{FluentArgs, FluentResource, concurrent::FluentBundle};
use std::{collections::BTreeMap, sync::OnceLock};
use unic_langid::{CharacterDirection, LanguageIdentifier};

pub use language::{Language, current_language, set_language};
pub use message::{Argument, Message};

struct LocaleDefinition {
    code: &'static str,
    native_name: &'static str,
    resources: &'static [&'static str],
}

include!(concat!(env!("OUT_DIR"), "/i18n_catalogs.rs"));

struct Catalog {
    bundle: FluentBundle<FluentResource>,
    plain: BTreeMap<String, String>,
}

impl Catalog {
    fn new(code: &str, resources: &[&str], static_messages: &[&str]) -> Self {
        let locale: LanguageIdentifier = code.parse().expect("validated locale identifier");
        let isolate = locale.character_direction() == CharacterDirection::RTL;
        let mut bundle = FluentBundle::new_concurrent(vec![locale]);
        bundle.add_builtins().expect("unique Fluent builtin names");
        // Keep technical values free of invisible marks in LTR interfaces while
        // isolating embedded values when a right-to-left catalog is selected.
        bundle.set_use_isolating(isolate);
        for source in resources {
            let resource = FluentResource::try_new((*source).to_owned())
                .expect("validated embedded Fluent resource");
            bundle
                .add_resource(resource)
                .expect("unique embedded Fluent identifiers");
        }
        let mut catalog = Self {
            bundle,
            plain: BTreeMap::new(),
        };
        for id in static_messages {
            if let Some(text) = catalog.format(id, None) {
                catalog.plain.insert((*id).to_owned(), text);
            }
        }
        catalog
    }

    fn format(&self, id: &str, arguments: Option<&FluentArgs<'_>>) -> Option<String> {
        let message = self.bundle.get_message(id)?;
        let pattern = message.value()?;
        let mut errors = Vec::new();
        let text = self.bundle.format_pattern(pattern, arguments, &mut errors);
        debug_assert!(errors.is_empty(), "invalid Fluent message {id}: {errors:?}");
        Some(text.into_owned())
    }
}

fn catalog(language: Language) -> &'static Catalog {
    static CATALOGS: OnceLock<Vec<OnceLock<Catalog>>> = OnceLock::new();
    let catalogs = CATALOGS.get_or_init(|| LOCALES.iter().map(|_| OnceLock::new()).collect());
    let index = language.locale_index();
    catalogs[index].get_or_init(|| {
        let locale = &LOCALES[index];
        Catalog::new(locale.code, locale.resources, STATIC_MESSAGES)
    })
}

/// Translate an identifier without arguments. Untranslated messages use the
/// English catalog; unknown dynamic text is returned unchanged.
pub fn tr(id: &str) -> &str {
    text_in(current_language(), id)
}

fn text_in(language: Language, id: &str) -> &str {
    catalog(language)
        .plain
        .get(id)
        .or_else(|| catalog(Language::English).plain.get(id))
        .map_or(id, String::as_str)
}

/// Format a message with typed Fluent arguments. Numeric arguments retain their
/// numeric type for locale-specific plural selection.
pub fn format(id: &str, arguments: &[(&str, Argument)]) -> String {
    format_in(current_language(), id, arguments)
}

fn format_in(language: Language, id: &str, arguments: &[(&str, Argument)]) -> String {
    let mut values = FluentArgs::with_capacity(arguments.len());
    for (name, argument) in arguments {
        values.set(*name, argument.value_in(language));
    }
    catalog(language)
        .format(id, Some(&values))
        .or_else(|| catalog(Language::English).format(id, Some(&values)))
        .unwrap_or_else(|| id.to_owned())
}

/// Format a message immediately. Use [`msg!`](crate::msg) for text retained in
/// application state so changing the language can update it later.
#[macro_export]
macro_rules! trf {
    ($id:expr $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::format(
            $id,
            &[$((stringify!($name), $crate::i18n::Argument::from(&$value))),*],
        )
    };
}

/// Retain a translation identifier and typed arguments until the text is shown.
#[macro_export]
macro_rules! msg {
    ($id:expr $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::Message::with_args(
            $id,
            vec![$((stringify!($name), $crate::i18n::Argument::from(&$value))),*],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluent_uses_locale_plural_rules_and_literal_parameter_values() {
        let source = "count = { $count ->\n    [one] one\n    [few] few\n   *[other] other\n}\npath = { $path }";
        let english = Catalog::new("en", &[source], &[]);
        let russian = Catalog::new("ru", &[source], &[]);
        for (catalog, count, expected) in [
            (&english, 1, "one"),
            (&english, 2, "other"),
            (&russian, 1, "one"),
            (&russian, 2, "few"),
            (&russian, 5, "other"),
        ] {
            let mut arguments = FluentArgs::new();
            arguments.set("count", count);
            assert_eq!(catalog.format("count", Some(&arguments)).unwrap(), expected);
        }
        let mut arguments = FluentArgs::new();
        arguments.set("path", "/tmp/{ $count }/file");
        assert_eq!(
            english.format("path", Some(&arguments)).unwrap(),
            "/tmp/{ $count }/file"
        );
    }

    #[test]
    fn static_messages_resolve_terms_once() {
        let catalog = Catalog::new(
            "en",
            &["-app = RustTerm\nwelcome = Welcome to { -app }"],
            &["welcome"],
        );
        assert_eq!(catalog.plain["welcome"], "Welcome to RustTerm");
        assert!(!catalog.plain.contains_key("unknown"));
        assert_eq!(text_in(Language::English, "unknown"), "unknown");
    }

    #[test]
    fn number_function_is_available_in_cached_and_parameterized_messages() {
        let source = "fixed = { NUMBER(1) }\nrank = { NUMBER($count, type: \"ordinal\") ->\n    [two] { $count }nd\n   *[other] { $count }th\n}";
        let catalog = Catalog::new("en", &[source], &["fixed"]);
        assert_eq!(catalog.plain["fixed"], "1");
        for (count, expected) in [(12, "12th"), (22, "22nd")] {
            let mut arguments = FluentArgs::new();
            arguments.set("count", count);
            assert_eq!(catalog.format("rank", Some(&arguments)).unwrap(), expected);
        }
    }

    #[test]
    fn embedded_catalogs_select_count_forms() {
        for (language, count, expected) in [
            (Language::English, 1, "1 minute ago"),
            (Language::English, 2, "2 minutes ago"),
            (Language::SimplifiedChinese, 1, "1 分钟前"),
        ] {
            assert_eq!(
                format_in(
                    language,
                    messages::COMMON_TIME_MINUTES_AGO,
                    &[("count", count.into())],
                ),
                expected
            );
        }
    }

    #[test]
    fn embedded_catalogs_preserve_significant_whitespace() {
        for (language, count, expected) in [
            (Language::English, 1, "\nand 1 more item"),
            (Language::English, 2, "\nand 2 more items"),
            (Language::SimplifiedChinese, 1, "\n以及另外 1 项"),
        ] {
            assert_eq!(
                format_in(
                    language,
                    messages::SFTP_ADDITIONAL_ITEM_COUNT,
                    &[("count", count.into())],
                ),
                expected
            );
        }
        assert_eq!(
            text_in(
                Language::English,
                messages::TERMINAL_STATUS_ALTERNATE_SCREEN
            ),
            " · Alternate screen"
        );
        assert_eq!(
            text_in(Language::English, messages::COMMON_DETAIL_SEPARATOR),
            "; "
        );
    }

    #[test]
    fn embedded_templates_never_interpret_parameter_contents() {
        let detail = r#"/tmp/{ $count }/{ -term }/"quoted"\n{ }"#;
        for (language, prefix) in [
            (Language::English, "Could not create window: "),
            (Language::SimplifiedChinese, "窗口创建失败："),
        ] {
            assert_eq!(
                format_in(
                    language,
                    messages::COMMON_COULD_NOT_CREATE_WINDOW_ERROR,
                    &[("error", detail.into())],
                ),
                format!("{prefix}{detail}")
            );
        }
    }
}
