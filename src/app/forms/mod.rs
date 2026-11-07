use rustterm::i18n::messages as i18n_key;
mod profile;
mod settings;
pub use profile::{CredentialChange, ProfileForm, ProfileKind};
pub use settings::{LockChange, RestartForUpdate, SettingsForm, SettingsSection};

use crate::app::{text_input::TextInput, ui};
use gpui::{AnyElement, App, Context, Div, Entity, Window, div, prelude::*, px, rgb};
use rustterm::i18n::{Message, tr};
use std::str::FromStr;

/// Plain named fields remain useful for the small group editor. Keep the raw
/// initial text, so invalid drafts are still recognized as unsaved changes.
pub struct Fields(pub Vec<(&'static str, Entity<TextInput>)>, Vec<String>);
impl Fields {
    pub fn new<T: 'static>(values: Vec<(&'static str, String)>, cx: &mut Context<T>) -> Self {
        let fields = values
            .into_iter()
            .map(|(key, value)| (key, input(key, value, cx)))
            .collect();
        let mut result = Self(fields, Vec::new());
        result.1 = result.values(cx);
        result
    }
    fn field(&self, key: &str) -> &Entity<TextInput> {
        &self
            .0
            .iter()
            .find(|(label, _)| *label == key)
            .expect("known form field")
            .1
    }
    pub fn get(&self, key: &str, cx: &App) -> String {
        self.field(key).read(cx).value().to_owned()
    }
    fn values(&self, cx: &App) -> Vec<String> {
        self.0
            .iter()
            .map(|(_, field)| field.read(cx).value().to_owned())
            .collect()
    }
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.values(cx) != self.1
    }
    pub fn inputs(&self) -> Vec<Entity<TextInput>> {
        self.0.iter().map(|(_, input)| input.clone()).collect()
    }
    fn selected_inputs(&self, keys: &[&str]) -> Vec<Entity<TextInput>> {
        keys.iter().map(|key| self.field(key).clone()).collect()
    }
    pub fn rows(&self) -> AnyElement {
        column()
            .children(
                self.0
                    .iter()
                    .map(|(label, input)| row(label, input.clone())),
            )
            .into_any_element()
    }
    fn selected_rows(&self, keys: &[&str]) -> AnyElement {
        column()
            .children(keys.iter().map(|key| {
                let (label, input) = self.0.iter().find(|(label, _)| label == key).unwrap();
                row(label, input.clone())
            }))
            .into_any_element()
    }
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some((_, input)) = self.0.first() {
            input.read(cx).focus(window);
        }
    }
}

fn input<T: 'static>(
    placeholder: &'static str,
    value: impl AsRef<str>,
    cx: &mut Context<T>,
) -> Entity<TextInput> {
    cx.new(|cx| {
        let mut input = TextInput::new(placeholder, false, cx);
        input.set_value(value, cx);
        input
    })
}
fn column() -> Div {
    div().flex().flex_col().gap(px(8.))
}
fn row(label: &'static str, content: impl IntoElement) -> Div {
    ui::form_row(tr(label), content)
}
fn hint(source: &'static str) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(ui::colors().muted))
        .child(tr(source))
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct FieldError {
    field: &'static str,
    message: Message,
}

fn field_error(field: &'static str, message: impl Into<Message>) -> anyhow::Error {
    FieldError {
        field,
        message: message.into(),
    }
    .into()
}

fn parse_value<T: FromStr>(label: &'static str, text: &str) -> anyhow::Result<T> {
    text.trim().parse().map_err(|_| {
        field_error(
            label,
            rustterm::msg!(
                i18n_key::CONNECTIONS_INVALID_VALUE_FOR_FIELD,
                field = Message::new(label)
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{FieldError, i18n_key, parse_value};

    #[test]
    fn numeric_parse_failures_retain_the_input_identity() {
        for value in ["", "abc", "-1", "65536"] {
            let error = parse_value::<u16>(i18n_key::WORKSPACE_PORT, value).unwrap_err();
            assert_eq!(
                error.downcast_ref::<FieldError>().unwrap().field,
                i18n_key::WORKSPACE_PORT
            );
        }
        assert_eq!(
            parse_value::<u16>(i18n_key::WORKSPACE_PORT, " 22 ").unwrap(),
            22
        );
    }
}
