use super::local::{StartupError, StartupField};
use super::{ProfileForm, ProfileKind};
use crate::app::forms::FieldError;
use gpui::{Context, Window};
use rustterm::i18n::messages as i18n_key;

impl ProfileForm {
    /// Present a local validation failure at its input; connection failures stay global.
    pub fn reveal_validation_error(
        &mut self,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(error) = error.downcast_ref::<StartupError>() {
            let input = match error.field {
                StartupField::Argument(index) => {
                    self.arguments.get(index).map(|row| row.value.clone())
                }
                StartupField::EnvironmentKey(index) => {
                    self.environment.get(index).map(|row| row.key.clone())
                }
                StartupField::EnvironmentValue(index) => {
                    self.environment.get(index).map(|row| row.value.clone())
                }
            };
            if let Some(input) = input {
                self.advanced = true;
                for input in self.inputs(cx) {
                    input.update(cx, |input, cx| input.clear_validation_error(cx));
                }
                input.update(cx, |input, cx| {
                    input.set_validation_error(error.message.clone(), cx)
                });
                input.read(cx).focus(window);
                cx.notify();
                return true;
            }
        }
        let (field, message) = if let Some(error) = error.downcast_ref::<FieldError>() {
            (error.field, error.message.clone())
        } else if let Some(rustterm::config::ConfigError::FieldValidation { field, message }) =
            error.downcast_ref::<rustterm::config::ConfigError>()
        {
            (*field, message.clone())
        } else {
            return false;
        };
        let input = if field == i18n_key::CONNECTIONS_PASSWORD_OR_PRIVATE_KEY_PASSPHRASE {
            self.credentials.input.clone()
        } else if let Some((_, input)) = self.fields.0.iter().find(|(label, _)| *label == field) {
            input.clone()
        } else {
            return false;
        };
        for (_, field) in &self.fields.0 {
            field.update(cx, |field, cx| field.clear_validation_error(cx));
        }
        self.credentials
            .input
            .update(cx, |input, cx| input.clear_validation_error(cx));
        if self.kind == ProfileKind::Rdp {
            self.reveal_rdp_field(field);
        } else if self.advanced_fields().contains(&field) {
            self.advanced = true;
        }
        input.update(cx, |input, cx| input.set_validation_error(message, cx));
        input.read(cx).focus(window);
        cx.notify();
        true
    }
}
