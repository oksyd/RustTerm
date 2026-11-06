//! Editable drafts remain alive until their configuration is durably saved.
use super::locking;
use super::persistence::profile_reference;
use super::{LeaveAction, Modal, Workspace};
use crate::app::forms::{
    ProfileForm, ProfileKind, RestartForUpdate, SettingsForm, SettingsSection,
};
use crate::app::text_input::{InputEvent, TextInput};
use gpui::prelude::*;
use gpui::{App, Context, Entity, PromptLevel, Window};
use rustterm::config::{AppConfig, Connection, Group, Profile, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, set_language, tr};
use rustterm::session::Session;
use std::time::Duration;

mod credential_commit;

impl Workspace {
    pub(super) fn open_settings(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lock_screen.locked
            || self.closing
            || self.modal_saving
            || self.authentication.read(cx).is_active()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if let Some(Modal::Settings(form)) = &self.modal {
            if matches!(
                section,
                SettingsSection::Security | SettingsSection::Updates
            ) {
                form.update(cx, |form, cx| form.select_section(section, window, cx));
            } else {
                form.read(cx).focus(window, cx);
            }
            return;
        }
        if self.modal.is_some() {
            return;
        }
        self.remember_popup_focus(window, cx);
        self.menu = None;
        self.palette.open = false;
        let form = cx.new(|cx| {
            SettingsForm::new(
                &self.config.settings,
                &self.config.app_lock,
                self.updates.clone(),
                cx,
            )
        });
        form.update(cx, |form, cx| form.select_section(section, window, cx));
        self.update_restart_subscription = Some(cx.subscribe_in(
            &form,
            window,
            |this, _, _: &RestartForUpdate, window, cx| {
                if !matches!(
                    this.updates.read(cx).status,
                    crate::app::updates::Status::Installed
                ) {
                    return;
                }
                if this.modal_dirty(cx) {
                    this.error = Some(i18n_key::UPDATES_SAVE_BEFORE_RESTART.into());
                    cx.notify();
                    return;
                }
                this.request_exit(window, cx);
                this.restart_requested = this.confirming_close;
            },
        ));
        self.modal = Some(Modal::Settings(form));
        self.error = self.last_save_error.clone();
        cx.notify();
    }

    pub(super) fn modal_inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        match &self.modal {
            Some(Modal::Auth {
                auto_connect: true, ..
            }) => Vec::new(),
            Some(Modal::Profile(form)) => form.read(cx).inputs(cx),
            Some(Modal::Settings(form)) => form.read(cx).inputs(cx),
            Some(Modal::Group { fields, .. }) => fields.inputs(),
            Some(Modal::Auth {
                secret,
                sentinel_secret,
                tunnel,
                ..
            }) => {
                let mut inputs = vec![secret.clone()];
                inputs.extend(sentinel_secret.iter().cloned());
                if let Some(tunnel) = tunnel {
                    inputs.push(tunnel.secret.clone());
                }
                inputs
            }
            _ => Vec::new(),
        }
    }
    pub(super) fn modal_dirty(&self, cx: &App) -> bool {
        match &self.modal {
            Some(Modal::Profile(form)) => form.read(cx).is_dirty(cx),
            Some(Modal::Settings(form)) => form.read(cx).is_dirty(cx),
            Some(Modal::Group { fields, .. }) => fields.is_dirty(cx),
            _ => false,
        }
    }
    pub(super) fn dismiss_modal(&mut self, cx: &mut Context<Self>) {
        self.cancel_profile_test();
        self.clear_recovery_credentials();
        // Theme selection is a visual preview. A successful save updates the
        // config first; every other dismissal restores the committed choice.
        crate::app::ui::set_theme(self.config.settings.app_theme, cx);
        self.modal = None;
        self.modal_observer = None;
        self.update_restart_subscription = None;
        self.modal_bound = false;
        self.modal_scroll = gpui::ScrollHandle::new();
        self.modal_generation += 1;
        self.modal_subscriptions.clear();
        self.bound_modal.clear();
    }
    pub(super) fn bind_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self.modal_inputs(cx);
        let newly_opened = self.modal.is_some() && !self.modal_bound;
        self.modal_bound = self.modal.is_some();
        if newly_opened && inputs.is_empty() && !self.window_state.prompts.is_busy() {
            self.restore_focus(window, cx);
        }
        if self.modal_observer.is_none() {
            self.modal_observer = match &self.modal {
                Some(Modal::Profile(form)) => Some(cx.observe(form, |this, _, cx| {
                    this.cancel_profile_test();
                    cx.notify();
                })),
                Some(Modal::Settings(form)) => Some(cx.observe(form, |_, _, cx| cx.notify())),
                _ => None,
            };
        }
        let ids: Vec<_> = inputs.iter().map(Entity::entity_id).collect();
        if ids == self.bound_modal {
            return;
        }
        self.bound_modal = ids;
        self.modal_subscriptions.clear();
        let scroll = match &self.modal {
            Some(Modal::Settings(form)) => form.read(cx).scroll_handle(),
            _ => self.modal_scroll.clone(),
        };
        for input in &inputs {
            input.update(cx, |input, _| input.set_scroll_container(scroll.clone()));
            self.modal_subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, input, event, window, cx| {
                    if matches!(event, InputEvent::Changed) {
                        this.cancel_profile_test();
                        // Manual edits revoke autofill, including typing and
                        // then clearing a credential while the vault is loading.
                        this.pending_credential_autofill.remove(&input.entity_id());
                        this.revoke_recovery_credential(input.entity_id());
                        cx.notify();
                        return;
                    }
                    if !matches!(event, InputEvent::Submit)
                        || this.window_state.is_locked()
                        || this.modal_composing(cx)
                        || this.closing
                        || this.modal_saving
                    {
                        return;
                    }
                    if matches!(this.modal, Some(Modal::Auth { .. })) {
                        this.authenticate(window, cx);
                    } else {
                        this.save_modal(window, cx);
                    }
                },
            ));
        }
        if newly_opened && let Some(input) = inputs.first() {
            input.read(cx).focus(window);
        }
    }
    pub(super) fn cycle_modal_focus(&self, backwards: bool, window: &mut Window, cx: &App) {
        if backwards {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        if !self.modal_focus.contains_focused(window, cx) {
            if backwards {
                let can_save = match &self.modal {
                    Some(Modal::Settings(form)) => {
                        !self.modal_saving && (self.save_failed || form.read(cx).is_dirty(cx))
                    }
                    Some(Modal::Profile(_) | Modal::Group { .. } | Modal::Auth { .. }) => true,
                    _ => false,
                };
                if can_save {
                    self.modal_save_focus.focus(window);
                } else {
                    self.modal_cancel_focus.focus(window);
                }
            } else {
                self.modal_focus.focus(window);
                window.focus_next();
            }
        }
    }
    pub(super) fn modal_composing(&self, cx: &App) -> bool {
        self.modal_inputs(cx)
            .iter()
            .any(|input| input.read(cx).is_composing())
    }
    pub(super) fn edit_profile(
        &mut self,
        profile: Option<Profile>,
        kind: ProfileKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked() {
            return;
        }
        self.remember_popup_focus(window, cx);
        let form = cx.new(|cx| {
            ProfileForm::new(
                profile,
                kind,
                self.config.groups.clone(),
                self.config.profiles.clone(),
                self.window_state.prompts.clone(),
                cx,
            )
        });
        form.read(cx).focus(window, cx);
        self.modal = Some(Modal::Profile(form));
        self.error = None;
        cx.notify();
    }
    pub(super) fn request_leave_modal(
        &mut self,
        action: LeaveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked()
            || self.modal_saving
            || self.modal_composing(cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if !self.modal_dirty(cx) {
            if matches!(action, LeaveAction::CloseModal)
                && matches!(self.modal, Some(Modal::Auth { .. }))
            {
                self.cancel_reconnect_for_modal(cx);
            }
            self.finish_leaving_modal(action, window, cx);
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let generation = self.modal_generation;
        let response = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::WORKSPACE_SAVE_YOUR_CHANGES),
            Some(tr(i18n_key::WORKSPACE_THIS_FORM_CONTAINS_UNSAVED_CHANGES)),
            &[
                tr(i18n_key::WORKSPACE_KEEP_EDITING),
                tr(i18n_key::WORKSPACE_DISCARD_CHANGES),
                tr(i18n_key::COMMON_SAVE),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let response = response.await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    let valid = permit.is_valid();
                    drop(permit);
                    if !valid || generation != this.modal_generation {
                        return;
                    }
                    match response {
                        Ok(1) => this.finish_leaving_modal(action, window, cx),
                        Ok(2) => this.save_modal_then(action, window, cx),
                        _ => {
                            if let Some(input) = this.modal_inputs(cx).first() {
                                input.read(cx).focus(window);
                            }
                        }
                    }
                    cx.notify();
                })
            });
        })
        .detach();
    }
    fn finish_leaving_modal(
        &mut self,
        action: LeaveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_modal(cx);
        self.error = self.last_save_error.clone();
        self.restore_popup_focus(window, cx);
        if matches!(action, LeaveAction::Exit) {
            self.request_exit(window, cx);
        }
        cx.notify();
    }
    fn edited_config(&self, cx: &App) -> anyhow::Result<AppConfig> {
        let mut config = self.config.clone();
        match &self.modal {
            Some(Modal::Profile(form)) => {
                let profile = form.read(cx).profile(&self.config.profiles, cx)?;
                apply_profile(&mut config, profile);
            }
            Some(Modal::Settings(form)) => {
                let mut settings = form.read(cx).settings(cx)?;
                // Resizing a window while editing must not restore stale dimensions.
                settings.window_width = config.settings.window_width;
                settings.window_height = config.settings.window_height;
                settings.sidebar_visible = config.settings.sidebar_visible;
                settings.sidebar_width = config.settings.sidebar_width;
                settings.files_width = config.settings.files_width;
                config.settings = settings;
            }
            Some(Modal::Group {
                editing,
                parent,
                fields,
            }) => {
                let name = fields.get(i18n_key::WORKSPACE_GROUP_NAME, cx);
                if let Some(id) = editing {
                    let group = config
                        .groups
                        .iter_mut()
                        .find(|group| group.id == *id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(tr(i18n_key::WORKSPACE_THIS_GROUP_NO_LONGER_EXISTS))
                        })?;
                    group.name = name;
                } else {
                    let mut group = Group::new(name);
                    group.parent = *parent;
                    config.groups.push(group);
                }
            }
            _ => anyhow::bail!(tr(i18n_key::WORKSPACE_THERE_IS_NO_EDITABLE_FORM_OPEN)),
        }
        config.validate()?;
        Ok(config)
    }
    pub(super) fn save_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_failed
            && matches!(&self.modal, Some(Modal::Settings(form)) if !form.read(cx).is_dirty(cx))
        {
            return;
        }
        self.save_modal_then(LeaveAction::CloseModal, window, cx);
    }
    fn save_modal_then(
        &mut self,
        action: LeaveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.window_state.is_locked()
            || self.modal_saving
            || self.modal_composing(cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let changes = self.edited_config(cx).and_then(|_| match &self.modal {
            Some(Modal::Settings(form)) => Ok((form.read(cx).lock_change(cx)?, None)),
            Some(Modal::Profile(form)) => {
                let form = form.read(cx);
                Ok((None, Some((form.id(), form.credential_change(cx)?))))
            }
            _ => Ok((None, None)),
        });
        let (lock_change, credential_change) = match changes {
            Ok(change) => change,
            Err(error) => {
                let shown = if let Some(Modal::Profile(form)) = &self.modal {
                    form.update(cx, |form, cx| {
                        form.reveal_validation_error(&error, window, cx)
                    })
                } else {
                    false
                };
                self.error = (!shown).then(|| Message::from_error(error));
                if let Some(Modal::Settings(form)) = &self.modal
                    && let Some(section) = form.read(cx).validation_section(cx)
                {
                    form.update(cx, |form, cx| form.select_section(section, window, cx));
                }
                cx.notify();
                return;
            }
        };
        let changing_lock = lock_change.is_some();
        if changing_lock && !self.can_change_lock_settings(cx) {
            return;
        }
        self.cancel_profile_test();
        self.modal_saving = true;
        // The captured secrets survive an idle lock. Keep the draft intact for write retries.
        self.modal_focus.focus(window);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // Only saves and current credential writes can change the commit snapshot.
            // Vault reads, cleanup, and superseded writes need not delay this form.
            // The modal blocks new edits and authentication while this save is pending.
            while this
                .update(cx, |this, _| {
                    this.saving || !this.pending_credential_writes.is_empty()
                })
                .unwrap_or(false)
            {
                gpui::Timer::after(Duration::from_millis(20)).await;
            }
            let candidate = this.update(cx, |this, cx| {
                let candidate = this.edited_config(cx)?;
                this.saving = true;
                Ok::<_, anyhow::Error>((candidate, this.path.join("config.toml")))
            });
            let (mut candidate, path) = match candidate {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(error)) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.modal_saving = false;
                        this.error = Some(Message::from_error(error));
                        this.restore_focus(window, cx);
                        cx.notify();
                    });
                    return;
                }
                Err(_) => return,
            };
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    if let Some(change) = lock_change {
                        candidate.app_lock =
                            locking::resolve_lock_change(&candidate.app_lock, change)?;
                    }
                    Ok::<_, locking::LockChangeError>(credential_commit::save(
                        candidate,
                        credential_change,
                        &path,
                    ))
                })
                .await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.modal_saving = false;
                    this.saving = false;
                    let credential_commit::SaveOutcome {
                        config: candidate,
                        result,
                        unused_credential,
                    } = match prepared {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            this.reject_lock_change(error);
                            this.save_again = false;
                            if !this.lock_screen.locked
                                && let Some(Modal::Settings(form)) = &this.modal
                            {
                                form.update(cx, |form, cx| {
                                    form.select_section(SettingsSection::Security, window, cx)
                                });
                            }
                            this.restore_focus(window, cx);
                            cx.notify();
                            return;
                        }
                    };
                    let committed = result
                        .as_ref()
                        .map_or_else(|error| error.was_committed(), |_| true);
                    if changing_lock && committed {
                        this.apply_lock_settings(candidate.app_lock.clone(), window, cx);
                        if let Some(Modal::Settings(form)) = &this.modal {
                            form.update(cx, |form, cx| {
                                form.accept_lock_config(&candidate.app_lock, cx)
                            });
                        }
                    }
                    if committed {
                        for reference in this.config.profiles.iter().filter_map(profile_reference) {
                            if !candidate.uses_credential(reference) {
                                this.credential_cleanup.insert(reference);
                            }
                        }
                        let current_size = (
                            this.config.settings.window_width,
                            this.config.settings.window_height,
                        );
                        let workspace = this.config.workspace.clone();
                        this.config = candidate;
                        this.config.workspace = workspace;
                        this.config.settings.window_width = current_size.0;
                        this.config.settings.window_height = current_size.1;
                        if result.is_ok() {
                            this.persisted_credentials = this
                                .config
                                .profiles
                                .iter()
                                .filter_map(profile_reference)
                                .collect();
                        } else {
                            // The rename happened. Keep that state and protect both sets of
                            // references until a later save confirms directory durability.
                            this.persisted_credentials
                                .extend(this.config.profiles.iter().filter_map(profile_reference));
                        }
                        this.updates.update(cx, |updates, cx| {
                            updates.configure(&this.config.settings, cx)
                        });
                        set_language(this.config.settings.language);
                        crate::app::ui::set_theme(this.config.settings.app_theme, cx);
                        Session::set_global_history_budget(
                            this.config.settings.global_history_bytes,
                        );
                        for pane in this.panes.values() {
                            pane.apply_settings(&this.config.settings, cx);
                        }
                        for files in this.files.values() {
                            files.update(cx, |files, cx| files.refresh_language(cx));
                        }
                        this.session_filter.update(cx, |_, cx| cx.notify());
                    }
                    if let Some(reference) = unused_credential {
                        this.credential_cleanup.insert(reference);
                    }
                    match result {
                        Ok(()) => {
                            this.save_failed = false;
                            this.last_save_error = None;
                            this.error = None;
                            this.cleanup_credentials(cx);
                            this.save_confirmation = Some(cx.spawn(async move |this, cx| {
                                gpui::Timer::after(Duration::from_secs(3)).await;
                                let _ = this.update(cx, |this, cx| {
                                    this.save_confirmation = None;
                                    cx.notify();
                                });
                            }));
                            this.finish_leaving_modal(action, window, cx);
                        }
                        Err(error) => {
                            let message =
                                if matches!(&error, credential_commit::SaveError::Configuration(_))
                                {
                                    this.save_failed = true;
                                    let message = rustterm::msg!(
                                        i18n_key::WORKSPACE_CONFIGURATION_NEEDS_ATTENTION_ERROR,
                                        error = Message::from_error(error)
                                    );
                                    this.last_save_error = Some(message.clone());
                                    message
                                } else {
                                    Message::from_error(error)
                                };
                            this.error = Some(message);
                            this.cleanup_credentials(cx);
                            if this.window_state.is_locked() {
                                this.focus_lock_password(window, cx);
                            } else if let Some(input) = this.modal_inputs(cx).first() {
                                input.read(cx).focus(window);
                            }
                        }
                    }
                    // No automatic writes can supersede an uncertain form commit.
                    this.save_again = false;
                    cx.notify();
                })
            });
        })
        .detach();
    }
}

/// A gateway identity change changes the destination behind a database address such
/// as localhost. Clear dependent secrets in the same durable config transaction.
fn apply_profile(config: &mut AppConfig, profile: Profile) {
    let changed_route = config.profiles.iter().find(|previous| previous.id == profile.id)
                    .is_some_and(|previous| matches!((&previous.connection, &profile.connection),
                        (Connection::Ssh(before), Connection::Ssh(after)) if !before.same_auth_identity(after)));
    if changed_route {
        for dependent in &mut config.profiles {
            if dependent.connection.ssh_profile() == Some(profile.id) {
                match &mut dependent.connection {
                    Connection::Redis(config) => config.credential = None,
                    Connection::Mysql(config) | Connection::Postgres(config) => {
                        config.credential = None
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(existing) = config.profiles.iter_mut().find(|p| p.id == profile.id) {
        *existing = profile;
    } else {
        config.profiles.push(profile);
    }
}

impl Workspace {
    pub(super) fn delete_profile(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.uses_ssh_profile(id) {
            self.error = Some(
                i18n_key::DATABASE_THIS_SSH_PROFILE_IS_USED_BY_A_DATABASE_TUNNEL_CHANGE.into(),
            );
            cx.notify();
            return;
        }
        let Some(profile) = self.config.profiles.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let response = window.prompt(
            PromptLevel::Warning,
            &rustterm::trf!(i18n_key::WORKSPACE_DELETE_PROFILE_NAME, name = profile.name),
            Some(tr(i18n_key::WORKSPACE_RUNNING_SESSIONS_ARE_UNAFFECTED_YOU_CAN_ALSO_REMOVE_CREDENTIALS_THAT)),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::WORKSPACE_DELETE_PROFILE_ONLY), tr(i18n_key::WORKSPACE_DELETE_PROFILE_AND_CREDENTIALS)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let answer = response.await.unwrap_or(0);
            if answer == 0 {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid();
                drop(permit);
                if !valid {
                    return;
                }
                if this.config.uses_ssh_profile(id) {
                    this.error = Some(
                        i18n_key::DATABASE_THIS_SSH_PROFILE_IS_USED_BY_A_DATABASE_TUNNEL_CHANGE
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                let removed = this
                    .config
                    .profiles
                    .iter()
                    .position(|profile| profile.id == id)
                    .map(|index| this.config.profiles.remove(index));
                if this.selected_profile == Some(id) {
                    this.selected_profile = None;
                }
                this.pending_credential_writes.remove(&id);
                if answer == 2
                    && let Some(reference) = removed.as_ref().and_then(profile_reference)
                {
                    this.credential_cleanup.insert(reference);
                }
                this.save(cx);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn delete_group(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let response = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::WORKSPACE_DELETE_GROUP_CONFIRMATION),
            Some(tr(i18n_key::WORKSPACE_DELETE_GROUP_KEEP_CHILDREN)),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_DELETE)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let confirmed = response.await == Ok(1);
            if confirmed {
                let _ = this.update(cx, |this, cx| {
                    let valid = permit.is_valid();
                    drop(permit);
                    if !valid {
                        return;
                    }
                    this.config.remove_group(id);
                    this.collapsed.remove(&id);
                    this.save(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::CredentialRef;
    use rustterm::config::{RedisConfig, SqlConfig, SshConfig};

    #[test]
    fn changing_gateway_identity_invalidates_only_dependent_database_credentials() {
        let mut gateway = Profile::ssh("Gateway", SshConfig::default());
        let reference = CredentialRef::new();
        let redis = Profile::redis(
            "Private Redis",
            RedisConfig {
                ssh_profile: Some(gateway.id),
                credential: Some(reference),
                ..Default::default()
            },
        );
        let direct = Profile::redis(
            "Direct Redis",
            RedisConfig {
                credential: Some(reference),
                ..Default::default()
            },
        );
        let mysql = SqlConfig {
            credential: Some(reference),
            ..SqlConfig::mysql()
        };
        let postgres = SqlConfig {
            credential: Some(reference),
            ..SqlConfig::postgres()
        };
        let mut config = AppConfig {
            profiles: vec![
                gateway.clone(),
                redis,
                Profile::mysql(
                    "Private MySQL",
                    SqlConfig {
                        ssh_profile: Some(gateway.id),
                        ..mysql.clone()
                    },
                ),
                Profile::postgres(
                    "Private PostgreSQL",
                    SqlConfig {
                        ssh_profile: Some(gateway.id),
                        ..postgres.clone()
                    },
                ),
                direct,
                Profile::mysql("Direct MySQL", mysql),
                Profile::postgres("Direct PostgreSQL", postgres),
            ],
            ..Default::default()
        };
        gateway.name = "Renamed gateway".into();
        apply_profile(&mut config, gateway.clone());
        for profile in &config.profiles[1..] {
            assert_eq!(profile_reference(profile), Some(reference));
        }
        let Connection::Ssh(ssh) = &mut gateway.connection else {
            unreachable!()
        };
        ssh.host = "other.example".into();
        apply_profile(&mut config, gateway);
        for profile in &config.profiles[1..4] {
            assert_eq!(profile_reference(profile), None);
        }
        for profile in &config.profiles[4..] {
            assert_eq!(profile_reference(profile), Some(reference));
        }
    }
}
