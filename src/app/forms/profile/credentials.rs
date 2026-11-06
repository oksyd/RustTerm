use super::super::{column, field_error, hint, row};
use super::{ProfileForm, ProfileKind, SshAuthentication};
use crate::app::{
    text_input::TextInput,
    ui::{self, ButtonStyle},
};
use gpui::{AnyElement, App, Context, Entity, div, prelude::*, rgb};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{CredentialRef, Profile, Secret},
    i18n::tr,
};

pub enum CredentialChange {
    Keep,
    Remove,
    Store(Secret),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Keep,
    Replace,
    Remove,
}

pub(super) struct CredentialEditor {
    pub(super) input: Entity<TextInput>,
    mode: Mode,
    cleared_on_lock: bool,
}

impl CredentialEditor {
    pub(super) fn new(cx: &mut Context<ProfileForm>) -> Self {
        Self {
            input: cx.new(|cx| {
                TextInput::new(
                    i18n_key::CONNECTIONS_PASSWORD_OR_PRIVATE_KEY_PASSPHRASE,
                    true,
                    cx,
                )
            }),
            mode: Mode::Keep,
            cleared_on_lock: false,
        }
    }

    pub(super) fn reset(&mut self, cx: &mut Context<ProfileForm>) {
        self.input.update(cx, |input, cx| input.clear(cx));
        self.mode = Mode::Keep;
        self.cleared_on_lock = false;
    }
}

impl ProfileForm {
    fn supports_credential(&self) -> bool {
        matches!(
            self.kind,
            ProfileKind::Redis | ProfileKind::Mysql | ProfileKind::Postgres | ProfileKind::Rdp
        ) || self.kind == ProfileKind::Ssh
            && matches!(
                self.ssh_auth,
                SshAuthentication::Password | SshAuthentication::PrivateKey
            )
    }

    fn saved_credential(&self, cx: &App) -> Option<CredentialRef> {
        let original = self.original.as_ref()?;
        let mut profiles = vec![original.clone()];
        if let Some(tunnel) = self
            .ssh_profile
            .and_then(|id| self.ssh_profiles.iter().find(|profile| profile.id == id))
        {
            profiles.push(tunnel.clone());
        }
        self.profile(&profiles, cx)
            .ok()
            .and_then(|profile| profile.connection.credential_ref())
    }

    pub(super) fn credential_input_visible(&self, cx: &App) -> bool {
        self.supports_credential()
            && self.credentials.mode != Mode::Remove
            && (self.credentials.mode == Mode::Replace
                || self.credentials.cleared_on_lock
                || !self.credentials.input.read(cx).value().is_empty()
                || self.saved_credential(cx).is_none())
    }

    pub(super) fn credential_dirty(&self, cx: &App) -> bool {
        self.supports_credential()
            && (self.credentials.mode != Mode::Keep
                || self.credentials.cleared_on_lock
                || !self.credentials.input.read(cx).value().is_empty())
    }

    pub fn credential_change(&self, cx: &App) -> anyhow::Result<CredentialChange> {
        if !self.supports_credential() {
            return Ok(CredentialChange::Keep);
        }
        let value = self.credentials.input.read(cx).value();
        if self.credentials.cleared_on_lock && value.is_empty() {
            return Err(field_error(
                i18n_key::CONNECTIONS_PASSWORD_OR_PRIVATE_KEY_PASSPHRASE,
                Message::new(
                    i18n_key::CONNECTIONS_THE_PASSWORD_WAS_CLEARED_WHEN_RUSTTERM_LOCKED_RE_ENTER_IT,
                ),
            ));
        }
        match self.credentials.mode {
            Mode::Remove => Ok(CredentialChange::Remove),
            Mode::Replace if value.is_empty() => Err(field_error(
                i18n_key::CONNECTIONS_PASSWORD_OR_PRIVATE_KEY_PASSPHRASE,
                Message::new(i18n_key::CONNECTIONS_ENTER_A_REPLACEMENT_PASSWORD_OR_CHOOSE_REMOVE_TO_STOP_SAVING),
            )),
            _ if !value.is_empty() => Ok(CredentialChange::Store(Secret::new(value.to_owned()))),
            _ => Ok(CredentialChange::Keep),
        }
    }

    pub fn secret_for_test(
        &self,
        current_profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<(Option<CredentialRef>, Option<Secret>)> {
        let profile = self.profile(current_profiles, cx)?;
        if !self.supports_credential() {
            return Ok((None, None));
        }
        let value = self.credentials.input.read(cx).value();
        if self.credentials.cleared_on_lock && value.is_empty() {
            return Err(field_error(
                i18n_key::CONNECTIONS_PASSWORD_OR_PRIVATE_KEY_PASSPHRASE,
                Message::new(
                    i18n_key::CONNECTIONS_THE_PASSWORD_WAS_CLEARED_WHEN_RUSTTERM_LOCKED_RE_ENTER_IT,
                ),
            ));
        }
        if self.credentials.mode == Mode::Keep && value.is_empty() {
            if let Some(reference) = profile.connection.credential_ref() {
                return Ok((Some(reference), None));
            }
            if self.saved_credential(cx).is_some() {
                anyhow::bail!(Message::new(
                    i18n_key::CONNECTIONS_THE_SAVED_CREDENTIAL_IS_NO_LONGER_AVAILABLE_CHOOSE_REPLACE_AND
                ));
            }
        }
        Ok((None, Some(Secret::new(value.to_owned()))))
    }

    pub fn clear_sensitive(&mut self, cx: &mut Context<Self>) {
        self.credentials.cleared_on_lock |= self.credentials.mode == Mode::Replace
            || !self.credentials.input.read(cx).value().is_empty();
        self.credentials
            .input
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    pub(super) fn credential_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.supports_credential() {
            return div().into_any_element();
        }
        let saved = self.saved_credential(cx).is_some();
        let label =
            if self.kind == ProfileKind::Ssh && self.ssh_auth == SshAuthentication::PrivateKey {
                i18n_key::CONNECTIONS_PRIVATE_KEY_PASSPHRASE
            } else {
                i18n_key::WORKSPACE_PASSWORD
            };
        let mut body = column();
        match self.credentials.mode {
            Mode::Keep
                if saved
                    && self.credentials.input.read(cx).value().is_empty()
                    && !self.credentials.cleared_on_lock =>
            {
                body = body.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(hint(i18n_key::CONNECTIONS_SAVED_CREDENTIAL_WILL_BE_KEPT))
                        .child(
                            ui::text_button(
                                "replace-profile-credential",
                                i18n_key::CONNECTIONS_REPLACE,
                                ButtonStyle::Neutral,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    if this.native_pending || this.prompts.is_blocked() {
                                        return;
                                    }
                                    this.credentials.mode = Mode::Replace;
                                    this.credentials.input.read(cx).focus(window);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            ui::text_button(
                                "remove-profile-credential",
                                "Remove",
                                ButtonStyle::Neutral,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.native_pending || this.prompts.is_blocked() {
                                    return;
                                }
                                this.credentials.reset(cx);
                                this.credentials.mode = Mode::Remove;
                                cx.notify();
                            })),
                        ),
                );
            }
            Mode::Remove => {
                body = body.child(hint(
                    i18n_key::CONNECTIONS_THE_SAVED_CREDENTIAL_WILL_BE_REMOVED_WHEN_YOU_SAVE_THIS,
                ));
            }
            _ => {
                body = body
                    .child(self.credentials.input.clone())
                    .child(hint(
                        i18n_key::CONNECTIONS_NON_EMPTY_PASSWORDS_ARE_SAVED_IN_THE_SYSTEM_CREDENTIAL_STORE,
                    ))
                    .child(hint(if self.credentials.mode == Mode::Replace {
                        i18n_key::CONNECTIONS_ENTER_A_REPLACEMENT_TO_SAVE_TESTING_WITH_AN_EMPTY_FIELD
                    } else {
                        i18n_key::CONNECTIONS_LEAVE_BLANK_TO_ENTER_A_PASSWORD_WHEN_CONNECTING_TESTING_WITH
                    }));
            }
        }
        if self.credentials.cleared_on_lock && self.credentials.input.read(cx).value().is_empty() {
            body = body.child(div().text_color(rgb(ui::colors().warning)).child(tr(
                i18n_key::CONNECTIONS_THE_PASSWORD_WAS_CLEARED_WHEN_RUSTTERM_LOCKED_RE_ENTER_IT,
            )));
        }
        if self.credentials.mode != Mode::Keep || self.credentials.cleared_on_lock {
            body = body.child(
                div().child(
                    ui::text_button(
                        "discard-profile-credential",
                        i18n_key::CONNECTIONS_DISCARD_PASSWORD_CHANGE,
                        ButtonStyle::Neutral,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.native_pending || this.prompts.is_blocked() {
                            return;
                        }
                        this.credentials.reset(cx);
                        cx.notify();
                    })),
                ),
            );
        }
        row(label, body).into_any_element()
    }
}
