use super::*;
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;

impl SettingsForm {
    pub fn clear_lock_secrets(&mut self, cx: &mut App) {
        self.lock.password_pending |= !self.lock.password.read(cx).value().is_empty()
            || !self.lock.confirm.read(cx).value().is_empty();
        for input in [&self.lock.current, &self.lock.password, &self.lock.confirm] {
            input.update(cx, |input, cx| input.clear(cx));
        }
    }

    pub fn accept_lock_config(&mut self, config: &AppLockConfig, cx: &mut Context<Self>) {
        self.clear_lock_secrets(cx);
        self.lock.password_pending = false;
        self.lock.password_editing = false;
        self.lock.configured = config.password_hash.is_some();
        self.lock.initial_idle = config.idle_minutes;
        self.lock.enabled = self.lock.configured;
        self.lock.automatic = !self.lock.configured || config.idle_minutes > 0;
        self.lock.idle.update(cx, |input, cx| {
            input.set_value(default_idle_minutes(config.idle_minutes).to_string(), cx)
        });
        cx.notify();
    }

    pub fn lock_change(&self, cx: &App) -> anyhow::Result<Option<LockChange>> {
        let password = self.lock.password.read(cx).value();
        let confirm = self.lock.confirm.read(cx).value();
        if self.lock.enabled {
            if self.lock.password_pending && password.is_empty() {
                anyhow::bail!(Message::new(
                    i18n_key::CONNECTIONS_PASSWORD_FIELDS_WERE_CLEARED_WHEN_RUSTTERM_LOCKED_RE_ENTER_THE
                ));
            }
            if password != confirm {
                anyhow::bail!(Message::new(
                    i18n_key::SETTINGS_THE_NEW_PASSWORDS_DO_NOT_MATCH
                ));
            }
            if (!self.lock.configured || !password.is_empty())
                && (password.chars().count() < app_lock::MIN_PASSWORD_CHARACTERS
                    || password.len() > app_lock::MAX_PASSWORD_BYTES)
            {
                anyhow::bail!(Message::new(
                    i18n_key::SETTINGS_USE_AT_LEAST_8_CHARACTERS_AND_AT_MOST_1024_BYTES
                ));
            }
        }
        let idle_minutes = lock_idle_minutes(
            self.lock.enabled,
            self.lock.automatic,
            self.lock.idle.read(cx).value(),
        )
        .ok_or_else(|| {
            anyhow::anyhow!(Message::new(
                i18n_key::CONNECTIONS_ENTER_AN_IDLE_TIMEOUT_FROM_1_TO_1440_MINUTES
            ))
        })?;
        if self.lock.enabled == self.lock.configured
            && idle_minutes == self.lock.initial_idle
            && (!self.lock.enabled || password.is_empty())
        {
            return Ok(None);
        }
        let current = self.lock.current.read(cx).value();
        if self.lock.configured && current.is_empty() {
            anyhow::bail!(Message::new(
                i18n_key::CONNECTIONS_ENTER_THE_CURRENT_LOCK_PASSWORD_TO_CHANGE_APPLICATION_LOCKING
            ));
        }
        Ok(Some(LockChange {
            current: Secret::new(current.to_owned()),
            password: Secret::new(if self.lock.enabled {
                password.to_owned()
            } else {
                String::new()
            }),
            enabled: self.lock.enabled,
            idle_minutes,
        }))
    }

    fn lock_password_fields_visible(&self) -> bool {
        self.lock.enabled
            && (!self.lock.configured || self.lock.password_editing || self.lock.password_pending)
    }

    fn lock_policy_changed(&self, cx: &App) -> bool {
        lock_idle_minutes(
            self.lock.enabled,
            self.lock.automatic,
            self.lock.idle.read(cx).value(),
        ) != Some(self.lock.initial_idle)
    }

    fn current_lock_password_required(&self, cx: &App) -> bool {
        self.lock.configured
            && (self.lock.enabled != self.lock.configured
                || self.lock.password_editing
                || self.lock.password_pending
                || self.lock_policy_changed(cx))
    }

    pub(super) fn lock_is_dirty(&self, cx: &App) -> bool {
        self.lock.enabled != self.lock.configured
            || (self.lock.enabled
                && (self.lock_policy_changed(cx)
                    || self.lock.password_pending
                    || !self.lock.password.read(cx).value().is_empty()
                    || !self.lock.confirm.read(cx).value().is_empty()))
    }

    pub(super) fn refresh_lock_inputs(&mut self, cx: &mut Context<Self>) {
        if !self.current_lock_password_required(cx)
            && !self.lock.current.read(cx).value().is_empty()
        {
            self.lock.current.update(cx, |input, cx| input.clear(cx));
        }
    }

    pub(super) fn security_inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        let mut inputs = Vec::new();
        if self.lock_password_fields_visible() {
            inputs.extend([self.lock.password.clone(), self.lock.confirm.clone()]);
        }
        if self.lock.enabled && self.lock.automatic {
            inputs.push(self.lock.idle.clone());
        }
        if self.current_lock_password_required(cx) {
            inputs.push(self.lock.current.clone());
        }
        inputs
    }

    pub(super) fn security(&self, cx: &Context<Self>) -> Div {
        let password_visible = self.lock_password_fields_visible();
        let mut body = column()
            .child(section_heading(i18n_key::CONNECTIONS_APPLICATION_LOCK))
            .child(
                toggle(
                    "enable-app-lock",
                    i18n_key::CONNECTIONS_ENABLE_APPLICATION_LOCK,
                    Some(match (self.lock.configured, self.lock.enabled) {
                        (true, true) => {
                            i18n_key::SETTINGS_PASSWORD_PROTECTED_ACTIVE_CONNECTIONS_KEEP_RUNNING
                        }
                        (true, false) => i18n_key::SETTINGS_LOCKING_WILL_BE_DISABLED_AFTER_SAVING,
                        (false, true) => i18n_key::SETTINGS_SET_A_PASSWORD_TO_ENABLE_LOCKING,
                        (false, false) => {
                            i18n_key::SETTINGS_HIDE_THE_WORKSPACE_WITHOUT_CLOSING_CONNECTIONS
                        }
                    }),
                    self.lock.enabled,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    if this
                        .inputs(cx)
                        .iter()
                        .any(|input| input.read(cx).is_composing())
                    {
                        return;
                    }
                    this.lock.enabled = !this.lock.enabled;
                    this.lock.password_pending = false;
                    this.lock.password_editing = false;
                    for input in [&this.lock.password, &this.lock.confirm] {
                        input.update(cx, |input, cx| input.clear(cx));
                    }
                    this.refresh_lock_inputs(cx);
                    this.focus(window, cx);
                    cx.notify();
                })),
            );
        if self.lock.enabled {
            if password_visible {
                body = body
                    .when(self.lock.password_pending, |body| body.child(hint(
                    i18n_key::CONNECTIONS_PASSWORD_FIELDS_WERE_CLEARED_WHEN_RUSTTERM_LOCKED_RE_ENTER_THE,
                    )))
                    .child(setting_row(
                        i18n_key::SETTINGS_NEW_LOCK_PASSWORD,
                        Some(i18n_key::SETTINGS_USE_AT_LEAST_8_CHARACTERS),
                        div().w(px(220.)).child(self.lock.password.clone()),
                    ))
                    .child(setting_row(
                        i18n_key::SETTINGS_CONFIRM_LOCK_PASSWORD,
                        None,
                        div().w(px(220.)).child(self.lock.confirm.clone()),
                    ))
                    .when(self.lock.configured, |body| body.child(
                        div().flex().justify_end().py_2().child(
                            ui::text_button("cancel-lock-password", i18n_key::SETTINGS_CANCEL_PASSWORD_CHANGE, ui::ButtonStyle::Neutral)
                                .tab_index(0)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.security_inputs(cx).iter().any(|input| input.read(cx).is_composing()) { return; }
                                    this.lock.password_editing = false;
                                    this.lock.password_pending = false;
                                    for input in [&this.lock.password, &this.lock.confirm] {
                                        input.update(cx, |input, cx| input.clear(cx));
                                    }
                                    this.refresh_lock_inputs(cx);
                                    this.section_focus[SettingsSection::Security as usize].focus(window);
                                    cx.notify();
                                })),
                        ),
                    ));
            } else {
                body = body.child(setting_row(
                    i18n_key::SETTINGS_LOCK_PASSWORD,
                    Some(i18n_key::SETTINGS_REQUIRED_WHEN_RUSTTERM_STARTS),
                    ui::text_button(
                        "change-lock-password",
                        i18n_key::SETTINGS_CHANGE_PASSWORD,
                        ui::ButtonStyle::Neutral,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this
                            .security_inputs(cx)
                            .iter()
                            .any(|input| input.read(cx).is_composing())
                        {
                            return;
                        }
                        this.lock.password_editing = true;
                        this.lock.password.read(cx).focus(window);
                        cx.notify();
                    })),
                ));
            }
            body = body
                .child(
                    toggle(
                        "automatic-app-lock",
                        i18n_key::CONNECTIONS_LOCK_AUTOMATICALLY_AFTER_INACTIVITY,
                        Some(i18n_key::SETTINGS_BACKGROUND_OUTPUT_DOES_NOT_RESET_THE_IDLE_TIMER),
                        self.lock.automatic,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.lock.idle.read(cx).is_composing() {
                            return;
                        }
                        this.lock.automatic = !this.lock.automatic;
                        if (!this.lock.automatic
                            && this.lock.idle.read(cx).focus_handle().is_focused(window))
                            || (!this.current_lock_password_required(cx)
                                && this.lock.current.read(cx).focus_handle().is_focused(window))
                        {
                            this.section_focus[SettingsSection::Security as usize].focus(window);
                        }
                        this.refresh_lock_inputs(cx);
                        cx.notify();
                    })),
                )
                .when(self.lock.automatic, |body| {
                    body.child(setting_row(
                        i18n_key::CONNECTIONS_IDLE_MINUTES,
                        None,
                        div().w(px(100.)).child(self.lock.idle.clone()),
                    ))
                });
        }
        if self.current_lock_password_required(cx) {
            body = body.child(setting_row(
                i18n_key::SETTINGS_CURRENT_LOCK_PASSWORD,
                Some(i18n_key::SETTINGS_REQUIRED_TO_SAVE_SECURITY_CHANGES),
                div().w(px(220.)).child(self.lock.current.clone()),
            ));
        }
        body.when(self.lock.enabled, |body| {
            body.child(
                div()
                    .pt_2()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::SETTINGS_LOCK_SHORTCUT))
                    .child("  Ctrl+Shift+L"),
            )
        })
        .child(hint(
            i18n_key::CONNECTIONS_APPLICATION_LOCK_CHANGES_TAKE_EFFECT_AFTER_SAVING,
        ))
        .child(section_heading(i18n_key::CONNECTIONS_CLIPBOARD))
        .child(
            toggle(
                "paste-toggle",
                i18n_key::CONNECTIONS_CONFIRM_MULTILINE_PASTE,
                Some(i18n_key::SETTINGS_ASK_BEFORE_PASTING_MULTIPLE_LINES_INTO_A_TERMINAL),
                self.values.confirm_multiline_paste,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.values.confirm_multiline_paste = !this.values.confirm_multiline_paste;
                cx.notify();
            })),
        )
    }
}

fn lock_idle_minutes(enabled: bool, automatic: bool, input: &str) -> Option<u32> {
    if enabled && automatic {
        input
            .trim()
            .parse()
            .ok()
            .filter(|minutes| (1..=1440).contains(minutes))
    } else {
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::lock_idle_minutes;

    #[test]
    fn idle_policy_ignores_hidden_drafts_and_validates_active_timeouts() {
        assert_eq!(lock_idle_minutes(false, true, "invalid"), Some(0));
        assert_eq!(lock_idle_minutes(true, false, "invalid"), Some(0));
        assert_eq!(lock_idle_minutes(true, true, " 010 "), Some(10));
        assert_eq!(lock_idle_minutes(true, true, "1440"), Some(1440));
        for input in ["", "invalid", "0", "1441", "-1"] {
            assert_eq!(lock_idle_minutes(true, true, input), None);
        }
    }
}
