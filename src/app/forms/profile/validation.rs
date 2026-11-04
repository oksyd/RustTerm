use super::{ProfileForm, ProfileKind, rdp, ssh::SSH_ADVANCED};
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
        if (self.kind == ProfileKind::Ssh && SSH_ADVANCED.contains(&field))
            || (self.kind == ProfileKind::Rdp && rdp::ADVANCED.contains(&field))
        {
            self.advanced = true;
        }
        input.update(cx, |input, cx| input.set_validation_error(message, cx));
        input.read(cx).focus(window);
        cx.notify();
        true
    }
}
