use super::{Language, current_language, format_in};
use fluent_bundle::FluentValue;
use std::{borrow::Cow, fmt, sync::Arc};

/// A value supplied to a Fluent message. Counts use Fluent numbers for plural
/// selection; integers beyond its exact numeric range remain decimal text.
#[derive(Clone, Debug, PartialEq)]
pub enum Argument {
    Text(String),
    Number(f64),
    Message(Box<Message>),
}

impl Argument {
    pub(super) fn value_in(&self, language: Language) -> FluentValue<'_> {
        match self {
            Self::Text(value) => FluentValue::from(value.as_str()),
            Self::Number(value) => FluentValue::from(*value),
            Self::Message(value) => FluentValue::from(value.render_in(language)),
        }
    }
}

impl From<String> for Argument {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Argument {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Cow<'_, str>> for Argument {
    fn from(value: Cow<'_, str>) -> Self {
        Self::Text(value.into_owned())
    }
}

impl From<bool> for Argument {
    fn from(value: bool) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<char> for Argument {
    fn from(value: char) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<Message> for Argument {
    fn from(value: Message) -> Self {
        Self::Message(Box::new(value))
    }
}

impl<T: Clone + Into<Argument>> From<&T> for Argument {
    fn from(value: &T) -> Self {
        value.clone().into()
    }
}

macro_rules! integer_arguments {
    (unsigned: $($unsigned:ty),*; signed: $($signed:ty),* $(;)?) => {
        $(impl From<$unsigned> for Argument {
            fn from(value: $unsigned) -> Self {
                if value as u128 <= 1_u128 << 53 {
                    Self::Number(value as f64)
                } else {
                    Self::Text(value.to_string())
                }
            }
        })*
        $(impl From<$signed> for Argument {
            fn from(value: $signed) -> Self {
                if (value as i128).unsigned_abs() <= 1_u128 << 53 {
                    Self::Number(value as f64)
                } else {
                    Self::Text(value.to_string())
                }
            }
        })*
    };
}

integer_arguments!(
    unsigned: u8, u16, u32, u64, u128, usize;
    signed: i8, i16, i32, i64, i128, isize;
);

impl From<f32> for Argument {
    fn from(value: f32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<f64> for Argument {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

/// Text retained by application state. Translated messages and nested arguments
/// are rendered with the current language, while external details stay literal.
#[derive(Clone, Debug, PartialEq)]
pub struct Message(Content);

#[derive(Clone)]
enum Content {
    Translated {
        id: &'static str,
        arguments: Vec<(&'static str, Argument)>,
    },
    Literal(String),
    Error(Arc<dyn fmt::Display + Send + Sync>),
    Joined {
        messages: Vec<Message>,
        separator: String,
    },
}

impl Message {
    pub fn new(id: &'static str) -> Self {
        Self::with_args(id, Vec::new())
    }

    pub fn with_arg(id: &'static str, name: &'static str, value: impl Into<Argument>) -> Self {
        Self::with_args(id, vec![(name, value.into())])
    }

    pub fn with_args(id: &'static str, arguments: Vec<(&'static str, Argument)>) -> Self {
        Self(Content::Translated { id, arguments })
    }

    /// Retain an error until display, including errors whose `Display`
    /// implementation translates their application-owned context.
    pub fn from_error(error: impl fmt::Display + Send + Sync + 'static) -> Self {
        Self(Content::Error(Arc::new(error)))
    }

    pub fn join(messages: impl IntoIterator<Item = Self>, separator: impl Into<String>) -> Self {
        Self(Content::Joined {
            messages: messages.into_iter().collect(),
            separator: separator.into(),
        })
    }

    pub fn render(&self) -> String {
        self.render_in(current_language())
    }

    fn render_in(&self, language: Language) -> String {
        match &self.0 {
            Content::Translated { id, arguments } => format_in(language, id, arguments),
            Content::Literal(text) => text.clone(),
            Content::Error(error) => error.to_string(),
            Content::Joined {
                messages,
                separator,
            } => messages
                .iter()
                .map(|message| message.render_in(language))
                .collect::<Vec<_>>()
                .join(separator),
        }
    }
}

impl From<&'static str> for Message {
    fn from(id: &'static str) -> Self {
        Self::new(id)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self(Content::Literal(text))
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Translated {
                    id: left,
                    arguments: left_args,
                },
                Self::Translated {
                    id: right,
                    arguments: right_args,
                },
            ) => left == right && left_args == right_args,
            (Self::Literal(left), Self::Literal(right)) => left == right,
            (Self::Error(left), Self::Error(right)) => Arc::ptr_eq(left, right),
            (
                Self::Joined {
                    messages: left,
                    separator: left_separator,
                },
                Self::Joined {
                    messages: right,
                    separator: right_separator,
                },
            ) => left == right && left_separator == right_separator,
            _ => false,
        }
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Translated { id, arguments } => f
                .debug_struct("Translated")
                .field("id", id)
                .field("arguments", arguments)
                .finish(),
            Self::Literal(text) => f.debug_tuple("Literal").field(text).finish(),
            Self::Error(error) => f
                .debug_tuple("Error")
                .field(&format_args!("{error}"))
                .finish(),
            Self::Joined {
                messages,
                separator,
            } => f
                .debug_struct("Joined")
                .field("messages", messages)
                .field("separator", separator)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::messages;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn retained_messages_render_in_the_requested_language() {
        let message = Message::new(messages::COMMON_FOLLOW_SYSTEM);
        assert_eq!(message.render_in(Language::English), "Follow system");
        assert_eq!(message.render_in(Language::SimplifiedChinese), "跟随系统");
        let nested = Argument::from(message.clone());
        assert_eq!(
            nested.value_in(Language::English),
            FluentValue::from("Follow system")
        );
        assert_eq!(
            nested.value_in(Language::SimplifiedChinese),
            FluentValue::from("跟随系统")
        );
        let joined = Message::join([message, Message::from(" /tmp/{file}".to_owned())], " · ");
        assert_eq!(
            joined.render_in(Language::English),
            "Follow system ·  /tmp/{file}"
        );
        assert_eq!(
            joined.render_in(Language::SimplifiedChinese),
            "跟随系统 ·  /tmp/{file}"
        );
    }

    #[test]
    fn error_display_is_deferred_and_shared_when_cloned() {
        struct CountingError(Arc<AtomicUsize>);
        impl fmt::Display for CountingError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fetch_add(1, Ordering::Relaxed);
                f.write_str("external detail")
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let message = Message::from_error(CountingError(count.clone()));
        let clone = message.clone();
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert_eq!(message, clone);
        assert_eq!(message.render(), "external detail");
        assert_eq!(clone.render(), "external detail");
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn nested_errors_keep_their_display_language_until_rendering() {
        struct LocalizedError(Arc<AtomicUsize>);
        impl fmt::Display for LocalizedError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let language = match self.0.load(Ordering::Relaxed) {
                    0 => Language::English,
                    _ => Language::SimplifiedChinese,
                };
                f.write_str(super::super::text_in(
                    language,
                    messages::COMMON_FOLLOW_SYSTEM,
                ))
            }
        }
        // A local selector keeps this test independent of concurrent tests using
        // the application's process-wide language preference.
        let language = Arc::new(AtomicUsize::new(0));
        let error = Message::from_error(LocalizedError(language.clone()));
        let message = Message::with_arg(
            messages::COMMON_COULD_NOT_CREATE_WINDOW_ERROR,
            "error",
            error,
        );
        assert_eq!(
            message.render_in(Language::English),
            "Could not create window: Follow system"
        );
        language.store(1, Ordering::Relaxed);
        assert_eq!(
            message.render_in(Language::SimplifiedChinese),
            "窗口创建失败：跟随系统"
        );
    }

    #[test]
    fn arguments_keep_numbers_and_macros_borrow_values() {
        let number = 42_usize;
        let text = String::from("literal");
        assert_eq!(Argument::from(&number), Argument::Number(42.0));
        assert_eq!(Argument::from(&&text), Argument::Text(text.clone()));
        let message = crate::msg!(messages::COMMON_COULD_NOT_CREATE_WINDOW_ERROR, error = text);
        assert_eq!(
            message.render_in(Language::English),
            "Could not create window: literal"
        );
        assert_eq!(text, "literal");
    }

    #[test]
    fn large_integer_arguments_preserve_every_digit() {
        const BOUNDARY: u64 = 1 << 53;
        assert_eq!(Argument::from(BOUNDARY), Argument::Number(BOUNDARY as f64));
        assert_eq!(
            Argument::from(-(BOUNDARY as i64)),
            Argument::Number(-(BOUNDARY as f64))
        );
        for value in [BOUNDARY + 1, u64::MAX] {
            assert_eq!(Argument::from(value), Argument::Text(value.to_string()));
        }
        for value in [-(BOUNDARY as i64) - 1, i64::MIN] {
            assert_eq!(Argument::from(value), Argument::Text(value.to_string()));
        }
        assert_eq!(
            Argument::from(u128::MAX),
            Argument::Text(u128::MAX.to_string())
        );
        assert_eq!(
            Argument::from(i128::MIN),
            Argument::Text(i128::MIN.to_string())
        );
    }
}
