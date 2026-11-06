use super::super::{column, field_error, hint, parse_value, row};
use super::{ProfileForm, preserved_path};
use crate::app::ui::{self, ButtonStyle, Icon};
use gpui::{AnyElement, App, Context, PathPromptOptions, div, prelude::*, px, rgb};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{AuthMethod, Connection, Profile, SshConfig, Uuid},
    i18n::tr,
};
use std::path::PathBuf;

pub(super) const SSH_ADVANCED: &[&str] = &[
    i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED,
    i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED,
    i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
    i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES,
    i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SshAuthentication {
    #[default]
    Password,
    PrivateKey,
    Agent,
    KeyboardInteractive,
}

pub(super) fn initial_fields(
    profile: Option<&Profile>,
    values: &mut Vec<(&'static str, String)>,
) -> SshAuthentication {
    let mut ssh_auth = SshAuthentication::Password;
    let settings = profile
        .as_ref()
        .and_then(|p| match &p.connection {
            Connection::Ssh(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let agent_socket = match &settings.auth {
        AuthMethod::Agent { socket } => socket
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        _ => String::new(),
    };
    let key_path = match &settings.auth {
        AuthMethod::PrivateKey { path, .. } => {
            ssh_auth = SshAuthentication::PrivateKey;
            path.display().to_string()
        }
        AuthMethod::Agent { .. } => {
            ssh_auth = SshAuthentication::Agent;
            String::new()
        }
        AuthMethod::KeyboardInteractive => {
            ssh_auth = SshAuthentication::KeyboardInteractive;
            String::new()
        }
        _ => String::new(),
    };
    values.extend([
        (
            i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL,
            agent_socket,
        ),
        (
            i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED,
            settings
                .jump_hosts
                .iter()
                .map(|hop| hop.address())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED,
            settings
                .forwards
                .iter()
                .map(|forward| forward.description())
                .collect::<Vec<_>>()
                .join("; "),
        ),
        (i18n_key::WORKSPACE_HOST, settings.host),
        (i18n_key::WORKSPACE_PORT, settings.port.to_string()),
        (i18n_key::WORKSPACE_USERNAME, settings.username),
        (i18n_key::CONNECTIONS_PRIVATE_KEY_PATH, key_path),
        (
            i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
            settings.timeout_secs.to_string(),
        ),
        (
            i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES,
            settings.keepalive_secs.to_string(),
        ),
        (
            i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT,
            settings.keepalive_max.to_string(),
        ),
    ]);
    ssh_auth
}

impl ProfileForm {
    pub(super) fn ssh_config(
        &self,
        current_profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<SshConfig> {
        let get = |key| self.fields.get(key, cx);
        let mut config = SshConfig {
            host: get(i18n_key::WORKSPACE_HOST),
            port: parse_value(i18n_key::WORKSPACE_PORT, &get(i18n_key::WORKSPACE_PORT))?,
            username: get(i18n_key::WORKSPACE_USERNAME),
            auth: if self.ssh_auth == SshAuthentication::PrivateKey {
                let text = get(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH);
                // Preserve a pre-existing non-UTF-8 path when its displayed text
                // was not edited, instead of silently replacing its bytes.
                let original_path = self.original.as_ref().and_then(|p| match &p.connection {
                    Connection::Ssh(SshConfig {
                        auth: AuthMethod::PrivateKey { path, .. },
                        ..
                    }) => Some(path),
                    _ => None,
                });
                AuthMethod::PrivateKey {
                    path: preserved_path(&text, original_path),
                    passphrase: None,
                }
            } else {
                match self.ssh_auth {
                    SshAuthentication::Agent => AuthMethod::Agent {
                        socket: (!get(i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL)
                            .trim()
                            .is_empty())
                        .then(|| {
                            PathBuf::from(get(i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL))
                        }),
                    },
                    SshAuthentication::KeyboardInteractive => AuthMethod::KeyboardInteractive,
                    _ => AuthMethod::Password { credential: None },
                }
            },
            jump_hosts: self.jump_hosts(cx).map_err(|error| {
                field_error(
                    i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED,
                    Message::from_error(error),
                )
            })?,
            forwards: rustterm::config::ssh::parse_forwards(&get(
                i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED,
            ))
            .map_err(|error| {
                field_error(
                    i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED,
                    Message::from_error(error),
                )
            })?,
            timeout_secs: parse_value(
                i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                &get(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
            )?,
            keepalive_secs: parse_value(
                i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES,
                &get(i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES),
            )?,
            keepalive_max: parse_value(
                i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT,
                &get(i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT),
            )?,
            shell_integration: self.shell_integration,
        };
        preserve_current_credential_reference(
            &mut config,
            self.original.as_ref().map(|profile| profile.id),
            current_profiles,
        );
        config.validate()?;
        Ok(config)
    }

    fn choose_key(&mut self, cx: &mut Context<Self>) {
        if self.native_pending || self.ssh_auth != SshAuthentication::PrivateKey {
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        let previous = self.fields.get(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH, cx);
        self.native_pending = true;
        self.picker_error = None;
        let response = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr(i18n_key::CONNECTIONS_SELECT_A_PRIVATE_KEY).into()),
        });
        cx.spawn(async move |this, cx| {
            let response = response.await;
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if !valid {
                    cx.notify();
                    return;
                }
                match response {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            if let Some(path) = path.to_str().filter(|path| !path.chars().any(char::is_control)) {
                                if this.ssh_auth == SshAuthentication::PrivateKey && this.fields.get(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH, cx) == previous {
                                    this.fields.field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH).update(cx, |input, cx| input.set_value(path, cx));
                                } else {
                                    this.picker_error = Some(i18n_key::CONNECTIONS_THE_PRIVATE_KEY_PATH_CHANGED_WHILE_THE_PICKER_WAS_OPEN);
                                }
                            } else {
                                this.picker_error = Some(i18n_key::CONNECTIONS_THE_SELECTED_PATH_CANNOT_BE_ENTERED_AS_TEXT_CHOOSE_A);
                            }
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => this.picker_error = Some(i18n_key::COMMON_COULD_NOT_OPEN_THE_FILE_PICKER_PLEASE_TRY_AGAIN),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn jump_hosts(&self, cx: &App) -> anyhow::Result<Vec<rustterm::config::SshJumpHost>> {
        let parsed = rustterm::config::ssh::parse_jump_hosts(
            &self
                .fields
                .get(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED, cx),
            &self.fields.get(i18n_key::WORKSPACE_USERNAME, cx),
        )?;
        let source = self.imported_ssh.as_ref().or_else(|| {
            self.original.as_ref().and_then(|p| {
                if let Connection::Ssh(ssh) = &p.connection {
                    Some(ssh)
                } else {
                    None
                }
            })
        });
        Ok(parsed
            .into_iter()
            .map(|hop| {
                source
                    .and_then(|s| {
                        s.jump_hosts.iter().find(|old| {
                            old.host == hop.host
                                && old.port == hop.port
                                && old.username == hop.username
                        })
                    })
                    .cloned()
                    .unwrap_or(hop)
            })
            .collect())
    }

    fn import_ssh(&mut self, cx: &mut Context<Self>) {
        if self.native_pending || self.prompts.is_blocked() {
            return;
        }
        let alias = self.fields.get(i18n_key::WORKSPACE_HOST, cx);
        let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        else {
            self.ssh_import_error =
                Some(Message::new(i18n_key::CONNECTIONS_HOME_IS_NOT_CONFIGURED));
            cx.notify();
            return;
        };
        self.native_pending = true;
        self.ssh_import_error = None;
        let task = cx.background_executor().spawn(async move {
            rustterm::config::ssh::import_openssh(&PathBuf::from(home).join(".ssh/config"), &alias)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.native_pending = false;
                if this.prompts.is_blocked() {
                    return;
                }
                match result {
                    Ok(config) => {
                        let (auth, key, agent) = match &config.auth {
                            AuthMethod::Password { .. } => {
                                (SshAuthentication::Password, String::new(), String::new())
                            }
                            AuthMethod::PrivateKey { path, .. } => (
                                SshAuthentication::PrivateKey,
                                path.display().to_string(),
                                String::new(),
                            ),
                            AuthMethod::Agent { socket } => (
                                SshAuthentication::Agent,
                                String::new(),
                                socket
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default(),
                            ),
                            AuthMethod::KeyboardInteractive => (
                                SshAuthentication::KeyboardInteractive,
                                String::new(),
                                String::new(),
                            ),
                        };
                        this.credentials.reset(cx);
                        this.ssh_auth = auth;
                        for (label, value) in [
                            (i18n_key::WORKSPACE_HOST, config.host.clone()),
                            (i18n_key::WORKSPACE_PORT, config.port.to_string()),
                            (i18n_key::WORKSPACE_USERNAME, config.username.clone()),
                            (i18n_key::CONNECTIONS_PRIVATE_KEY_PATH, key),
                            (i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL, agent),
                            (
                                i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                                config.timeout_secs.to_string(),
                            ),
                            (
                                i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES,
                                config.keepalive_secs.to_string(),
                            ),
                            (
                                i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT,
                                config.keepalive_max.to_string(),
                            ),
                            (
                                i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED,
                                config
                                    .jump_hosts
                                    .iter()
                                    .map(|h| h.address())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            ),
                            (
                                i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED,
                                config
                                    .forwards
                                    .iter()
                                    .map(|f| f.description())
                                    .collect::<Vec<_>>()
                                    .join(i18n_key::COMMON_DETAIL_SEPARATOR),
                            ),
                        ] {
                            this.fields
                                .field(label)
                                .update(cx, |input, cx| input.set_value(value, cx));
                        }
                        this.imported_ssh = Some(config);
                    }
                    Err(error) => this.ssh_import_error = Some(Message::from_error(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ssh_body(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .child(
                ui::text_button(
                    "import-openssh",
                    i18n_key::CONNECTIONS_READ_HOST_FROM_SSH_CONFIG,
                    ButtonStyle::Neutral,
                )
                .tab_index(0)
                .on_click(cx.listener(|this, _, _, cx| this.import_ssh(cx))),
            )
            .when_some(self.ssh_import_error.clone(), |body, error| {
                body.child(div().text_color(rgb(ui::colors().danger)).child(error.render()))
            })
            .when(self.ssh_auth == SshAuthentication::Agent, |body| {
                body.child(self.fields.selected_rows(&[i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL]))
            })
            .child(self.fields.selected_rows(&[i18n_key::WORKSPACE_HOST, i18n_key::WORKSPACE_PORT, i18n_key::WORKSPACE_USERNAME]))
            .child(row(
                i18n_key::COMMON_AUTHENTICATION,
                div().flex().gap_1().children(
                    [
                        (SshAuthentication::Password, i18n_key::WORKSPACE_PASSWORD),
                        (SshAuthentication::PrivateKey, i18n_key::COMMON_PRIVATE_KEY),
                        (SshAuthentication::Agent, i18n_key::CONNECTIONS_SSH_AGENT),
                        (SshAuthentication::KeyboardInteractive, i18n_key::CONNECTIONS_INTERACTIVE_MFA),
                    ]
                    .into_iter()
                    .map(|(auth, label)| {
                        ui::text_button(
                            label,
                            label,
                            if self.ssh_auth == auth {
                                ButtonStyle::Primary
                            } else {
                                ButtonStyle::Neutral
                            },
                        )
                        .tab_index(0)
                        .border_1()
                        .border_color(rgb(ui::colors().border))
                        .focus(|style| style.border_color(rgb(ui::colors().accent)))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.native_pending || this.prompts.is_blocked() {
                                    return;
                                }
                                if this.ssh_auth != auth {
                                    this.credentials.reset(cx);
                                }
                                this.ssh_auth = auth;
                                this.picker_error = None;
                                if auth == SshAuthentication::PrivateKey {
                                    this.fields.field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH).read(cx).focus(window);
                                } else {
                                    this.fields.field(i18n_key::WORKSPACE_USERNAME).read(cx).focus(window);
                                }
                                cx.notify();
                            },
                        ))
                    }),
                ),
            ))
            .when(self.ssh_auth == SshAuthentication::PrivateKey, |body| {
                body.child(row(
                    i18n_key::CONNECTIONS_PRIVATE_KEY_PATH,
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(self.fields.field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH).clone()),
                        )
                        .child(
                            ui::text_button_enabled(
                                "choose-private-key",
                                if self.native_pending {
                                    i18n_key::CONNECTIONS_SELECTING
                                } else {
                                    i18n_key::COMMON_BROWSE_DIALOG
                                },
                                ButtonStyle::Neutral,
                                !self.native_pending,
                            )
                            .border_1()
                            .border_color(rgb(ui::colors().border))
                            .focus(|style| style.border_color(rgb(ui::colors().accent)))
                            .on_click(cx.listener(|this, _, _, cx| this.choose_key(cx))),
                        ),
                ))
            })
            .when_some(self.picker_error, |body, error| {
                body.child(div().text_color(rgb(ui::colors().danger)).child(tr(error)))
            })
            .child(self.credential_editor(cx))
            .child(
                div()
                    .id("ssh-advanced")
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_color(rgb(ui::colors().muted))
                    .child(ui::icon(if self.advanced {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    }))
                    .child(tr(i18n_key::CONNECTIONS_ADVANCED_SSH_SETTINGS))
                    .tab_index(0)
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .focus(|style| style.border_color(rgb(ui::colors().accent)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.advanced
                            && this
                                .fields
                                .selected_inputs(SSH_ADVANCED)
                                .iter()
                                .any(|input| input.read(cx).focus_handle().is_focused(window))
                        {
                            this.fields.field(i18n_key::WORKSPACE_USERNAME).read(cx).focus(window);
                        }
                        this.advanced = !this.advanced;
                        cx.notify();
                    })),
            )
            .when(self.advanced, |body| {
                body.child(row(
                    i18n_key::CONNECTIONS_SHELL_INTEGRATION,
                    div()
                        .id("ssh-shell-integration")
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .px_2()
                        .cursor_pointer()
                        .tab_index(0)
                        .border_1()
                        .border_color(rgb(ui::colors().border))
                        .focus(|style| style.border_color(rgb(ui::colors().accent)))
                        .child(
                            div()
                                .size(px(14.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .border_1()
                                .rounded(px(2.))
                                .border_color(rgb(if self.shell_integration {
                                    ui::colors().accent
                                } else {
                                    ui::colors().border
                                }))
                                .when(self.shell_integration, |checkbox| {
                                    checkbox.child(ui::icon(Icon::Check).text_color(rgb(ui::colors().accent)))
                                }),
                        )
                        .child(tr(i18n_key::CONNECTIONS_LOAD_SHELL_INTEGRATION_AUTOMATICALLY))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shell_integration = !this.shell_integration;
                            cx.notify();
                        })),
                ))
                    .child(hint(i18n_key::CONNECTIONS_AUTOMATICALLY_INTEGRATES_BASH_ZSH_AND_FISH_WITHOUT_CHANGING_YOUR_STARTUP))
                    .child(self.fields.selected_rows(SSH_ADVANCED))
                    .child(hint(i18n_key::CONNECTIONS_JUMP_HOSTS_USE_SSH_AGENT_IDENTITIES_UNLESS_IMPORTED_WITH_A))
                    .child(hint(i18n_key::CONNECTIONS_L_127_0_0_1_8080_HOST_80_R_127))
            })
            .into_any_element()
    }
}

fn preserve_current_credential_reference(
    current: &mut SshConfig,
    profile_id: Option<Uuid>,
    profiles: &[Profile],
) {
    // A vault operation may finish while the form is open. Read the current
    // reference at submission, never resurrect the form's stale reference.
    if let Some(Profile {
        connection: Connection::Ssh(previous),
        ..
    }) = profile_id.and_then(|id| profiles.iter().find(|profile| profile.id == id))
    {
        preserve_credential_reference(current, previous);
    }
}

fn preserve_credential_reference(current: &mut SshConfig, previous: &SshConfig) {
    // A different destination, user, method or key must never inherit a secret.
    if previous.same_auth_identity(current) {
        current.auth = previous.auth.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::{preserve_credential_reference, preserve_current_credential_reference};
    use rustterm::config::{AuthMethod, Connection, CredentialRef, Profile, SshConfig, Uuid};

    #[test]
    fn credentials_follow_authentication_identity_only() {
        let reference = Some(CredentialRef(Uuid::from_u128(1)));
        let password = AuthMethod::Password { credential: None };
        let old = SshConfig {
            host: "server.test".into(),
            username: "alice".into(),
            auth: AuthMethod::Password {
                credential: reference,
            },
            ..SshConfig::default()
        };
        let mut same = SshConfig {
            auth: password.clone(),
            timeout_secs: 42,
            ..old.clone()
        };
        preserve_credential_reference(&mut same, &old);
        assert_eq!(same.auth, old.auth);
        for field in 0..3 {
            let mut changed = SshConfig {
                auth: password.clone(),
                ..old.clone()
            };
            match field {
                0 => changed.host = "another.test".into(),
                1 => changed.port = 2222,
                _ => changed.username = "bob".into(),
            }
            preserve_credential_reference(&mut changed, &old);
            assert_eq!(changed.auth, password);
        }
        let key = AuthMethod::PrivateKey {
            path: "/keys/id_ed25519".into(),
            passphrase: None,
        };
        let mut switched = SshConfig {
            auth: key.clone(),
            ..old.clone()
        };
        preserve_credential_reference(&mut switched, &old);
        assert_eq!(switched.auth, key);
        let previous_key = SshConfig {
            auth: AuthMethod::PrivateKey {
                path: "/keys/id_ed25519".into(),
                passphrase: reference,
            },
            ..old
        };
        preserve_credential_reference(&mut switched, &previous_key);
        assert_eq!(switched.auth, previous_key.auth);
        let other_key = AuthMethod::PrivateKey {
            path: "/keys/other".into(),
            passphrase: None,
        };
        switched.auth = other_key.clone();
        preserve_credential_reference(&mut switched, &previous_key);
        assert_eq!(switched.auth, other_key);
        switched.auth = password.clone();
        preserve_credential_reference(&mut switched, &previous_key);
        assert_eq!(switched.auth, password);
    }

    #[test]
    fn submitting_a_draft_uses_the_latest_credential_reference() {
        let profile_id = Uuid::from_u128(1);
        let latest_reference = Some(CredentialRef(Uuid::from_u128(3)));
        let current_ssh = SshConfig {
            host: "server.test".into(),
            username: "alice".into(),
            auth: AuthMethod::Password {
                credential: latest_reference,
            },
            ..SshConfig::default()
        };
        let profiles = vec![Profile {
            id: profile_id,
            name: "server".into(),
            group: None,
            connection: Connection::Ssh(current_ssh.clone()),
        }];
        let mut draft = SshConfig {
            auth: AuthMethod::Password { credential: None },
            ..current_ssh
        };
        preserve_current_credential_reference(&mut draft, Some(profile_id), &profiles);
        assert_eq!(
            draft.auth,
            AuthMethod::Password {
                credential: latest_reference
            }
        );
        draft.auth = AuthMethod::Password { credential: None };
        preserve_current_credential_reference(&mut draft, Some(profile_id), &[]);
        assert_eq!(draft.auth, AuthMethod::Password { credential: None });
    }
}
