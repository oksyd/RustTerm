//! Connection launch and per-service credentials. Database SSH forwarding uses a
//! snapshot of the selected SSH profile; it never reuses a terminal's PTY.
use super::persistence::profile_reference;
use super::{Modal, TunnelAuth, Workspace};
use crate::app::database_view::DatabaseLaunch;
use crate::app::elasticsearch_view::ElasticsearchLaunch;
use crate::app::layout::Axis;
use crate::app::rdp_view::RdpLaunch;
use crate::app::redis_view::RedisLaunch;
use crate::app::text_input::TextInput;
use gpui::prelude::*;
use gpui::{Context, Entity, PromptLevel, Window};
use rustterm::config::CredentialStore;
use rustterm::config::{
    AuthMethod, Connection, CredentialRef, KnownHosts, ProcessCredentialStore, Profile, Secret,
    SshConfig, Uuid,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};
use rustterm::session::{Session, SshAuth, SshOptions};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rustterm::config::credentials::CredentialError;

impl Workspace {
    pub(super) fn auth_credentials_loading(&self) -> bool {
        matches!(&self.modal, Some(Modal::Auth { secret, tunnel, .. })
            if self.pending_credential_autofill.contains(&secret.entity_id())
                || tunnel.as_ref().is_some_and(|tunnel|
                    self.pending_credential_autofill.contains(&tunnel.secret.entity_id())))
    }

    pub(super) fn remember_rdp_certificate(
        &mut self,
        index: usize,
        fingerprint: &str,
        cx: &mut Context<Self>,
    ) {
        // A form commit replaces the in-memory config with its saved snapshot.
        // Merge trust after that commit rather than letting the snapshot erase it.
        if self.modal_saving {
            let original = self.pane_profiles.get(&index).cloned();
            let fingerprint = fingerprint.to_owned();
            cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(Duration::from_millis(50)).await;
                    let waiting = this
                        .update(cx, |this, cx| {
                            let same_instance = this
                                .pane_profiles
                                .get(&index)
                                .zip(original.as_ref())
                                .is_some_and(|(current, original)| {
                                    current.id == original.id
                                        && current.connection == original.connection
                                });
                            if this.closing || !same_instance {
                                return false;
                            }
                            if this.modal_saving {
                                return true;
                            }
                            // Preserve an uncertain disk commit and its visible error.
                            if this.save_failed {
                                return false;
                            }
                            this.remember_rdp_certificate(index, &fingerprint, cx);
                            false
                        })
                        .unwrap_or(false);
                    if !waiting {
                        break;
                    }
                }
            })
            .detach();
            return;
        }
        let accepted = self.pane_profiles.get(&index).is_some_and(|original| {
            self.config
                .profiles
                .iter_mut()
                .any(|current| trust_rdp_certificate(original, current, fingerprint))
        });
        if accepted {
            // The normal save path reports persistence errors; the view must not
            // claim durable trust until that asynchronous write has succeeded.
            self.save(cx);
        } else {
            self.error = Some(
                i18n_key::RDP_THE_RDP_CONNECTION_CHANGED_WHILE_CONFIRMING_ITS_CERTIFICATE_TRUST_WAS
                    .into(),
            );
        }
        cx.notify();
    }

    pub(super) fn open_profile(
        &mut self,
        profile: Profile,
        reconnect: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_profile_at(profile, reconnect, None, window, cx);
    }

    pub(super) fn reconnect_with_credentials(
        &mut self,
        profile: Profile,
        id: usize,
        secret: Secret,
        tunnel: Option<(Profile, Secret)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ssh = tunnel.map(|(profile, secret)| {
            let Connection::Ssh(config) = profile.connection else {
                unreachable!("validated SSH route")
            };
            ssh_options(&config, &secret, self.path.join("known_hosts.toml"))
        });
        match &profile.connection {
            Connection::Ssh(config) => {
                // Automatic recovery never opens an interactive authentication prompt.
                let options = ssh_options(config, &secret, self.path.join("known_hosts.toml"));
                let session =
                    Session::spawn_ssh(options, 80, 24, self.config.settings.history_lines);
                self.add_session(profile, session, Some(id), None, window, cx);
            }
            Connection::Elasticsearch(config) => {
                let launch = ElasticsearchLaunch {
                    config: config.clone(),
                    secret,
                    ssh,
                };
                self.add_elasticsearch(profile, launch, Some(id), None, window, cx);
            }
            Connection::Redis(config) => {
                let launch = RedisLaunch {
                    config: config.clone(),
                    secret: (!secret.expose().is_empty()).then_some(secret),
                    sentinel_secret: None,
                    ssh,
                };
                self.add_redis(profile, launch, Some(id), None, window, cx);
            }
            Connection::Rdp(config) => {
                let launch = RdpLaunch {
                    config: config.clone(),
                    secret,
                };
                self.add_rdp(profile, launch, Some(id), None, window, cx);
            }
            Connection::Mysql(_) | Connection::Postgres(_) | Connection::Sqlite(_) => {
                let launch = DatabaseLaunch {
                    config: profile.connection.database().unwrap(),
                    secret,
                    ssh,
                };
                self.add_database(profile, launch, Some(id), None, window, cx);
            }
            Connection::Telnet(_) => self.open_profile(profile, Some(id), window, cx),
            Connection::Local(_) => unreachable!("local sessions do not automatically reconnect"),
        }
    }

    pub(super) fn open_profile_at(
        &mut self,
        profile: Profile,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing || self.window_state.is_locked() || self.modal.is_some() {
            return;
        }
        self.interrupt_restore_for_open();
        if self.panes.len() >= 64 && reconnect.is_none() {
            self.error =
                Some(i18n_key::WORKSPACE_THE_LIMIT_OF_64_RUNNING_PANES_HAS_BEEN_REACHED.into());
            cx.notify();
            return;
        }
        if let Connection::Local(local) = &profile.connection {
            let session = self.local_session(local);
            self.add_session(profile, session, reconnect, split, window, cx);
            return;
        }
        if matches!(profile.connection, Connection::Sqlite(_)) {
            let launch = DatabaseLaunch {
                config: profile.connection.database().unwrap(),
                secret: Secret::new(String::new()),
                ssh: None,
            };
            self.add_database(profile, launch, reconnect, split, window, cx);
            return;
        }
        if let Connection::Telnet(config) = &profile.connection {
            let session = Session::spawn_telnet(
                rustterm::session::TelnetOptions {
                    host: config.host.clone(),
                    port: config.port,
                    timeout_secs: config.timeout_secs,
                },
                80,
                24,
                self.config.settings.history_lines,
            );
            self.add_session(profile, session, reconnect, split, window, cx);
            return;
        }
        if let Connection::Ssh(config) = &profile.connection
            && matches!(
                config.auth,
                AuthMethod::Agent { .. } | AuthMethod::KeyboardInteractive
            )
        {
            let mut options = ssh_options(
                config,
                &Secret::new(String::new()),
                self.path.join("known_hosts.toml"),
            );
            options.authentication = Some(self.window_state.authentication.clone());
            let session = Session::spawn_ssh(options, 80, 24, self.config.settings.history_lines);
            self.add_session(profile, session, reconnect, split, window, cx);
            return;
        }
        let tunnel = match profile.connection.ssh_profile() {
            Some(id) => {
                let Some(ssh) = self
                    .config
                    .profiles
                    .iter()
                    .find(|profile| {
                        profile.id == id && matches!(profile.connection, Connection::Ssh(_))
                    })
                    .cloned()
                else {
                    self.error = Some(i18n_key::DATABASE_THE_SSH_PROFILE_FOR_THIS_DATABASE_CONNECTION_IS_UNAVAILABLE_EDIT.into());
                    cx.notify();
                    return;
                };
                Some(Box::new(TunnelAuth {
                    profile: ssh,
                    secret: cx.new(|cx| {
                        TextInput::new(
                            i18n_key::REDIS_SSH_TUNNEL_PASSWORD_PRIVATE_KEY_PASSPHRASE,
                            true,
                            cx,
                        )
                    }),
                    remember: false,
                }))
            }
            None => None,
        };
        let field = cx.new(|cx| {
            TextInput::new(
                match profile.connection {
                    Connection::Redis(_) => i18n_key::REDIS_REDIS_PASSWORD,
                    Connection::Elasticsearch(_) => i18n_key::ES_SECRET,
                    Connection::Mysql(_) | Connection::Postgres(_) => {
                        i18n_key::DATABASE_DATABASE_PASSWORD
                    }
                    Connection::Rdp(_) => i18n_key::RDP_RDP_PASSWORD,
                    _ => i18n_key::WORKSPACE_PASSWORD_PRIVATE_KEY_PASSPHRASE_FOR_THIS_CONNECTION,
                },
                true,
                cx,
            )
        });
        let mut references = vec![(profile_reference(&profile), field.clone())];
        if let Some(tunnel) = &tunnel {
            references.push((profile_reference(&tunnel.profile), tunnel.secret.clone()));
        }
        self.remember_popup_focus(window, cx);
        let sentinel_secret = matches!(&profile.connection, Connection::Redis(config) if config.topology == rustterm::config::RedisTopology::Sentinel)
            .then(|| cx.new(|cx| TextInput::new(i18n_key::REDIS_SENTINEL_PASSWORD_THIS_SESSION_ONLY, true, cx)));
        let auto_connect = can_connect_with_saved_credentials(
            &profile,
            tunnel.as_ref().map(|tunnel| &tunnel.profile),
        );
        self.modal = Some(Modal::Auth {
            profile: Box::new(profile),
            reconnect,
            split,
            secret: field,
            sentinel_secret,
            auto_connect,
            remember: false,
            tunnel,
        });
        for (reference, field) in references {
            if let Some(reference) = reference {
                self.load_credential(reference, field, window, cx);
            }
        }
        self.try_saved_authentication(window, cx);
        self.restore_focus(window, cx);
        cx.notify();
    }

    fn load_credential(
        &mut self,
        reference: CredentialRef,
        field: Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_credentials >= 4 {
            if let Some(Modal::Auth { auto_connect, .. }) = &mut self.modal {
                *auto_connect = false;
            }
            self.error = Some(
                i18n_key::REDIS_THE_CREDENTIAL_SERVICE_IS_BUSY_ENTER_THE_CREDENTIALS_MANUALLY_OR
                    .into(),
            );
            return;
        }
        self.pending_credentials += 1;
        let field_id = field.entity_id();
        self.pending_credential_autofill.insert(field_id);
        let task = cx
            .background_executor()
            .spawn(async move { ProcessCredentialStore::new().get(reference) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending_credentials = this.pending_credentials.saturating_sub(1);
                let autofill = this.pending_credential_autofill.remove(&field_id);
                this.cleanup_credentials(cx);
                let active = matches!(&this.modal, Some(Modal::Auth { secret, tunnel, .. })
                    if secret.entity_id() == field_id
                        || tunnel.as_ref().is_some_and(|tunnel| tunnel.secret.entity_id() == field_id));
                if autofill
                    && active
                    && field.read(cx).value().is_empty()
                    && !field.read(cx).is_composing()
                {
                    match result {
                        Ok(value) => {
                            field.update(cx, |input, cx| input.set_value(value.expose(), cx));
                            this.note_recovery_credential(field_id);
                        }
                        Err(error) => this.error = Some(Message::from_error(error)),
                    }
                    // An empty/missing vault entry must expose the form, not
                    // leave a loading screen waiting for an impossible launch.
                    if field.read(cx).value().is_empty()
                        && let Some(Modal::Auth { auto_connect, .. }) = &mut this.modal
                        && std::mem::take(auto_connect)
                    {
                        this.restore_focus(window, cx);
                    }
                    this.try_saved_authentication(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn authenticate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.window_state.is_locked() || self.modal_composing(cx) {
            return;
        }
        if self.auth_credentials_loading() {
            self.error = Some(
                i18n_key::REDIS_SAVED_CREDENTIALS_ARE_STILL_LOADING_WAIT_BEFORE_CONNECTING.into(),
            );
            cx.notify();
            return;
        }
        if let Some(Modal::Auth {
            profile,
            secret,
            tunnel,
            reconnect,
            split,
            ..
        }) = &self.modal
        {
            let missing_password = needs_password(profile) && secret.read(cx).value().is_empty()
                || tunnel.as_ref().is_some_and(|tunnel| {
                    needs_password(&tunnel.profile) && tunnel.secret.read(cx).value().is_empty()
                });
            if missing_password {
                self.error = Some(
                    i18n_key::WORKSPACE_ENTER_A_PASSWORD_NO_AUTHENTICATION_REQUEST_HAS_BEEN_SENT
                        .into(),
                );
                cx.notify();
                return;
            }
            let stale = reconnect.is_some_and(|id| !self.panes.contains_key(&id))
                || split.is_some_and(|(id, _)| !self.panes.contains_key(&id));
            if stale {
                self.error =
                    Some(i18n_key::WORKSPACE_THE_TARGET_PANE_WAS_CLOSED_OR_IS_TOO_SMALL_THE.into());
                self.dismiss_modal(cx);
                self.restore_popup_focus(window, cx);
                cx.notify();
                return;
            }
        }
        let Some(Modal::Auth {
            profile,
            reconnect,
            split,
            secret,
            sentinel_secret,
            remember,
            tunnel,
            ..
        }) = self.modal.take()
        else {
            return;
        };
        let profile = *profile;
        let value = Secret::new(secret.read(cx).value().to_owned());
        secret.update(cx, |field, cx| field.clear(cx));
        if remember && self.can_save_connection_credential(&profile) && !value.expose().is_empty() {
            self.remember_credential(&profile, Secret::new(value.expose().to_owned()), cx);
        }
        let ssh = tunnel.map(|tunnel| {
            let secret = Secret::new(tunnel.secret.read(cx).value().to_owned());
            tunnel.secret.update(cx, |field, cx| field.clear(cx));
            if tunnel.remember && !secret.expose().is_empty() {
                self.remember_credential(
                    &tunnel.profile,
                    Secret::new(secret.expose().to_owned()),
                    cx,
                );
            }
            let Connection::Ssh(config) = &tunnel.profile.connection else {
                unreachable!("validated SSH route")
            };
            let mut options = ssh_options(config, &secret, self.path.join("known_hosts.toml"));
            options.authentication = Some(self.window_state.authentication.clone());
            options
        });
        self.dismiss_modal(cx);
        self.popup_return_focus = None;
        match &profile.connection {
            Connection::Ssh(config) => {
                let mut options = ssh_options(config, &value, self.path.join("known_hosts.toml"));
                options.authentication = Some(self.window_state.authentication.clone());
                let session =
                    Session::spawn_ssh(options, 80, 24, self.config.settings.history_lines);
                self.add_session(profile, session, reconnect, split, window, cx);
            }
            Connection::Elasticsearch(config) => {
                let launch = ElasticsearchLaunch {
                    config: config.clone(),
                    secret: value,
                    ssh,
                };
                self.add_elasticsearch(profile, launch, reconnect, split, window, cx);
            }
            Connection::Redis(config) => {
                let launch = RedisLaunch {
                    config: config.clone(),
                    secret: (!value.expose().is_empty()).then_some(value),
                    sentinel_secret: sentinel_secret.and_then(|input| {
                        let secret = Secret::new(input.read(cx).value().to_owned());
                        input.update(cx, |input, cx| input.clear(cx));
                        (!secret.expose().is_empty()).then_some(secret)
                    }),
                    ssh,
                };
                self.add_redis(profile, launch, reconnect, split, window, cx);
            }
            Connection::Rdp(config) => {
                let launch = RdpLaunch {
                    config: config.clone(),
                    secret: value,
                };
                self.add_rdp(profile, launch, reconnect, split, window, cx);
            }
            Connection::Mysql(_) | Connection::Postgres(_) | Connection::Sqlite(_) => {
                let launch = DatabaseLaunch {
                    config: profile.connection.database().unwrap(),
                    secret: value,
                    ssh,
                };
                self.add_database(profile, launch, reconnect, split, window, cx);
            }
            Connection::Local(_) | Connection::Telnet(_) => {
                unreachable!("local and Telnet sessions do not use a credential dialog")
            }
        }
        self.restore_focus(window, cx);
    }

    fn remember_credential(&mut self, profile: &Profile, value: Secret, cx: &mut Context<Self>) {
        if self.pending_credentials < 4 {
            self.pending_credentials += 1;
            let original = profile.clone();
            let route = profile.connection.ssh_profile().and_then(|id| {
                self.config
                    .profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .map(|profile| (id, profile.connection.clone()))
            });
            let reference = CredentialRef::new();
            self.pending_credential_writes.insert(profile.id, reference);
            let task = cx
                .background_executor()
                .spawn(async move { ProcessCredentialStore::new().set(reference, &value) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let stored = result.is_ok();
                let accepted = this
                    .update(cx, |this, cx| {
                        this.pending_credentials = this.pending_credentials.saturating_sub(1);
                        let previous = this
                            .config
                            .profiles
                            .iter()
                            .find(|profile| profile.id == original.id)
                            .and_then(profile_reference);
                        let accepted = match complete_credential_write(
                            &mut this.pending_credential_writes,
                            &mut this.config.profiles,
                            &original,
                            reference,
                            route.as_ref(),
                            result,
                        ) {
                            Ok(true) => {
                                if let Some(old) = previous {
                                    this.credential_cleanup.insert(old);
                                }
                                this.save(cx);
                                true
                            }
                            Ok(false) => false,
                            Err(error) => {
                                this.error = Some(rustterm::msg!(
                                    i18n_key::WORKSPACE_ERROR_THIS_CONNECTION_WILL_USE_TEMPORARY_CREDENTIALS_THE_SAVED_CREDENTIAL,
                                    error = Message::from_error(error)
                                ));
                                false
                            }
                        };
                        cx.notify();
                        accepted
                    })
                    .unwrap_or(false);
                if stored && !accepted {
                    // The dialog/profile/window disappeared while the vault was writing.
                    cx.background_executor()
                        .spawn(async move {
                            let _ = ProcessCredentialStore::new().delete(reference);
                        })
                        .await;
                }
            })
            .detach();
        } else {
            self.error = Some(
                i18n_key::WORKSPACE_THE_CREDENTIAL_SERVICE_IS_BUSY_CREDENTIALS_WILL_BE_USED_FOR
                    .into(),
            );
        }
    }

    pub(super) fn can_save_connection_credential(&self, profile: &Profile) -> bool {
        let Some((engine, config)) = profile.connection.sql() else {
            return true;
        };
        self.config
            .profiles
            .iter()
            .find(|saved| saved.id == profile.id)
            .and_then(|saved| saved.connection.sql())
            .is_some_and(|(saved_engine, saved)| {
                engine == saved_engine && config.same_auth_identity(saved)
            })
    }
}

fn complete_credential_write(
    pending: &mut HashMap<Uuid, CredentialRef>,
    profiles: &mut [Profile],
    original: &Profile,
    reference: CredentialRef,
    route: Option<&(Uuid, Connection)>,
    result: Result<(), CredentialError>,
) -> Result<bool, CredentialError> {
    // Submission order owns the result, regardless of the vault's completion order.
    if pending.get(&original.id) != Some(&reference) {
        return Ok(false);
    }
    pending.remove(&original.id);
    result?;
    if !route_matches(route, profiles) {
        return Ok(false);
    }
    Ok(profiles
        .iter_mut()
        .find(|profile| profile.id == original.id)
        .is_some_and(|profile| attach_credential(profile, &original.connection, reference)))
}

/// Merge independent certificate/credential writes without reviving stale trust
/// or replacing a concurrently edited connection's settings.
fn attach_credential(
    profile: &mut Profile,
    original: &Connection,
    reference: CredentialRef,
) -> bool {
    let compatible = match (&profile.connection, original) {
        (Connection::Ssh(current), Connection::Ssh(before)) => current.same_auth_identity(before),
        (Connection::Rdp(current), Connection::Rdp(before)) => current.same_auth_identity(before),
        (Connection::Mysql(current), Connection::Mysql(before))
        | (Connection::Postgres(current), Connection::Postgres(before)) => {
            current.same_auth_identity(before)
        }
        (Connection::Elasticsearch(current), Connection::Elasticsearch(before)) => {
            current.same_auth_identity(before)
        }
        (Connection::Redis(current), Connection::Redis(before)) => {
            current.host == before.host
                && current.port == before.port
                && current.username == before.username
                && current.tls == before.tls
                && current.ssh_profile == before.ssh_profile
        }
        _ => false,
    };
    if !compatible {
        return false;
    }
    match &mut profile.connection {
        Connection::Ssh(config) => match &mut config.auth {
            AuthMethod::Password { credential } => *credential = Some(reference),
            AuthMethod::PrivateKey { passphrase, .. } => *passphrase = Some(reference),
            AuthMethod::Agent { .. } | AuthMethod::KeyboardInteractive => return false,
        },
        Connection::Redis(config) => config.credential = Some(reference),
        Connection::Elasticsearch(config) => config.credential = Some(reference),
        Connection::Mysql(config) | Connection::Postgres(config) => {
            config.credential = Some(reference)
        }
        Connection::Rdp(config) => config.credential = Some(reference),
        Connection::Local(_) | Connection::Telnet(_) | Connection::Sqlite(_) => return false,
    }
    true
}

fn trust_rdp_certificate(original: &Profile, current: &mut Profile, fingerprint: &str) -> bool {
    if original.id != current.id {
        return false;
    }
    let (Connection::Rdp(before), Connection::Rdp(after)) =
        (&original.connection, &mut current.connection)
    else {
        return false;
    };
    if !before.same_server(after) || before.certificate_sha256 != after.certificate_sha256 {
        return false;
    }
    after.certificate_sha256 = Some(fingerprint.to_owned());
    true
}

pub(super) fn needs_password(profile: &Profile) -> bool {
    matches!(&profile.connection, Connection::Ssh(config) if matches!(config.auth, AuthMethod::Password { .. }))
        || matches!(profile.connection, Connection::Rdp(_))
        || matches!(&profile.connection, Connection::Elasticsearch(config) if config.auth != rustterm::config::ElasticsearchAuth::None)
}

fn can_connect_with_saved_credentials(profile: &Profile, tunnel: Option<&Profile>) -> bool {
    (profile_reference(profile).is_some() || matches!(&profile.connection, Connection::Elasticsearch(config) if config.auth == rustterm::config::ElasticsearchAuth::None))
        // Sentinel has a separate, session-only password that the user may need
        // to supply even when the Redis data-node password is saved.
        && !matches!(&profile.connection, Connection::Redis(config)
            if config.topology == rustterm::config::RedisTopology::Sentinel)
        && tunnel.is_none_or(|profile| {
            profile_reference(profile).is_some() || credential_optional(profile)
        })
}

pub(super) fn credential_optional(profile: &Profile) -> bool {
    matches!(&profile.connection, Connection::Elasticsearch(config) if config.auth == rustterm::config::ElasticsearchAuth::None)
        || matches!(
            &profile.connection,
            Connection::Ssh(SshConfig {
                auth: AuthMethod::PrivateKey {
                    passphrase: None,
                    ..
                } | AuthMethod::Agent { .. }
                    | AuthMethod::KeyboardInteractive,
                ..
            }) | Connection::Redis(rustterm::config::RedisConfig {
                username: None,
                credential: None,
                ..
            })
        )
}

pub(super) fn ssh_options(
    config: &SshConfig,
    secret: &Secret,
    known_hosts_path: PathBuf,
) -> SshOptions {
    let auth = match &config.auth {
        AuthMethod::Password { .. } => SshAuth::Password(secret.expose().to_owned()),
        AuthMethod::PrivateKey { path, .. } => SshAuth::PrivateKey {
            path: path.clone(),
            passphrase: (!secret.expose().is_empty()).then(|| secret.expose().to_owned()),
        },
        AuthMethod::Agent { socket } => SshAuth::Agent {
            socket: socket.clone(),
        },
        AuthMethod::KeyboardInteractive => SshAuth::KeyboardInteractive,
    };
    SshOptions {
        host: config.host.clone(),
        port: config.port,
        username: config.username.clone(),
        auth,
        known_hosts_path,
        timeout_secs: config.timeout_secs,
        keepalive_secs: config.keepalive_secs,
        keepalive_max: config.keepalive_max,
        shell_integration: config.shell_integration,
        jump_hosts: config.jump_hosts.clone(),
        forwards: config.forwards.clone(),
        authentication: None,
    }
}

fn route_matches(route: Option<&(Uuid, Connection)>, profiles: &[Profile]) -> bool {
    let Some((id, Connection::Ssh(original))) = route else {
        return route.is_none();
    };
    profiles.iter().any(|profile| profile.id == *id
        && matches!(&profile.connection, Connection::Ssh(current) if original.same_auth_identity(current)))
}

impl Workspace {
    pub(super) fn open_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.remember_popup_focus(window, cx);
        self.dismiss_modal(cx);
        self.modal = Some(Modal::Loading);
        self.modal_focus.focus(window);
        let generation = self.modal_generation;
        let path = self.path.join("known_hosts.toml");
        let task = cx
            .background_executor()
            .spawn(async move { KnownHosts::load(path) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if this.modal_generation != generation
                        || !matches!(this.modal, Some(Modal::Loading))
                    {
                        return;
                    }
                    match result {
                        Ok(hosts) => this.modal = Some(Modal::Trust(hosts)),
                        Err(error) => {
                            this.error = Some(Message::from_error(error));
                            this.dismiss_modal(cx);
                            this.restore_popup_focus(window, cx);
                        }
                    }
                    cx.notify();
                })
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn forget_host(
        &mut self,
        host: String,
        port: u16,
        keys: Vec<rustterm::config::HostKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let response = window.prompt(
            PromptLevel::Warning,
            &rustterm::trf!(i18n_key::WORKSPACE_REMOVE_TRUSTED_KEYS_FOR_HOST_PORT, host = host, port = port),
            Some(tr(i18n_key::WORKSPACE_ONLY_THE_DISPLAYED_RECORDS_WILL_BE_REMOVED_VERIFY_THE_FINGERPRINT)),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::WORKSPACE_REMOVE_TRUST)],
            cx,
        );
        let path = self.path.join("known_hosts.toml");
        let generation = self.modal_generation;
        cx.spawn_in(window, async move |this, cx| {
            let accepted = response.await == Ok(1);
            let valid = permit.is_valid();
            drop(permit);
            if !accepted || !valid {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    KnownHosts::remove_if_unchanged_and_save(&path, &host, port, &keys)?;
                    KnownHosts::load(&path)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(hosts)
                        if this.modal_generation == generation
                            && matches!(this.modal, Some(Modal::Trust(_))) =>
                    {
                        this.modal = Some(Modal::Trust(hosts))
                    }
                    Ok(_) => {}
                    Err(error) => this.error = Some(Message::from_error(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::{
        ElasticsearchAuth, ElasticsearchConfig, RdpConfig, RedisConfig, RedisTopology, SqlConfig,
    };

    #[test]
    fn saved_passwords_start_connections_without_an_extra_authentication_form() {
        let mut profiles = [
            Profile::rdp("RDP", RdpConfig::default()),
            Profile::ssh(
                "SSH",
                SshConfig {
                    auth: AuthMethod::Password { credential: None },
                    ..Default::default()
                },
            ),
            Profile::redis("Redis", RedisConfig::default()),
            Profile::mysql("MySQL", SqlConfig::mysql()),
            Profile::postgres("PostgreSQL", SqlConfig::postgres()),
            Profile::elasticsearch(
                "Elasticsearch",
                ElasticsearchConfig {
                    auth: ElasticsearchAuth::ApiKey,
                    ..Default::default()
                },
            ),
        ];
        for profile in &mut profiles {
            assert!(
                !can_connect_with_saved_credentials(profile, None),
                "{}",
                profile.name
            );
            assert!(
                profile
                    .connection
                    .set_credential(Some(CredentialRef::new()))
            );
            assert!(
                can_connect_with_saved_credentials(profile, None),
                "{}",
                profile.name
            );
            assert!(profile.connection.set_credential(None));
            assert!(
                !can_connect_with_saved_credentials(profile, None),
                "{}",
                profile.name
            );
        }
    }

    #[test]
    fn unauthenticated_elasticsearch_skips_its_own_prompt_but_requires_tunnel_auth() {
        let profile = Profile::elasticsearch("ES", ElasticsearchConfig::default());
        assert!(can_connect_with_saved_credentials(&profile, None));
        assert!(credential_optional(&profile));
        assert!(!needs_password(&profile));
        let mut tunnel = Profile::ssh(
            "Gateway",
            SshConfig {
                auth: AuthMethod::Password { credential: None },
                ..Default::default()
            },
        );
        assert!(!can_connect_with_saved_credentials(&profile, Some(&tunnel)));
        tunnel.connection.set_credential(Some(CredentialRef::new()));
        assert!(can_connect_with_saved_credentials(&profile, Some(&tunnel)));
        for auth in [ElasticsearchAuth::Basic, ElasticsearchAuth::ApiKey] {
            let profile = Profile::elasticsearch(
                "ES",
                ElasticsearchConfig {
                    auth,
                    username: "operator".into(),
                    ..Default::default()
                },
            );
            assert!(!credential_optional(&profile));
            assert!(needs_password(&profile));
            assert!(!can_connect_with_saved_credentials(&profile, None));
        }
    }

    #[test]
    fn saved_database_password_does_not_skip_a_missing_tunnel_password() {
        let database = Profile::mysql(
            "database",
            SqlConfig {
                credential: Some(CredentialRef::new()),
                ..SqlConfig::mysql()
            },
        );
        let mut tunnel = Profile::ssh(
            "tunnel",
            SshConfig {
                auth: AuthMethod::Password { credential: None },
                ..Default::default()
            },
        );
        assert!(!can_connect_with_saved_credentials(
            &database,
            Some(&tunnel)
        ));
        tunnel.connection.set_credential(Some(CredentialRef::new()));
        assert!(can_connect_with_saved_credentials(&database, Some(&tunnel)));
        tunnel.connection.set_credential(None);
        assert!(!can_connect_with_saved_credentials(
            &database,
            Some(&tunnel)
        ));
    }

    #[test]
    fn tunnels_using_an_agent_or_unencrypted_key_do_not_need_a_password_form() {
        let database = Profile::postgres(
            "database",
            SqlConfig {
                credential: Some(CredentialRef::new()),
                ..SqlConfig::postgres()
            },
        );
        for auth in [
            AuthMethod::Agent { socket: None },
            AuthMethod::KeyboardInteractive,
            AuthMethod::PrivateKey {
                path: "id_ed25519".into(),
                passphrase: None,
            },
            AuthMethod::PrivateKey {
                path: "id_ed25519".into(),
                passphrase: Some(CredentialRef::new()),
            },
        ] {
            let tunnel = Profile::ssh(
                "tunnel",
                SshConfig {
                    auth,
                    ..Default::default()
                },
            );
            assert!(can_connect_with_saved_credentials(&database, Some(&tunnel)));
        }
    }

    #[test]
    fn sentinel_still_prompts_for_its_separate_session_only_credential() {
        let profile = Profile::redis(
            "Sentinel",
            RedisConfig {
                credential: Some(CredentialRef::new()),
                topology: RedisTopology::Sentinel,
                ..Default::default()
            },
        );
        assert!(!can_connect_with_saved_credentials(&profile, None));
    }

    #[test]
    fn latest_credential_submission_wins_in_either_completion_order() {
        let previous = CredentialRef::new();
        let originals = [
            Profile::elasticsearch(
                "Elasticsearch",
                ElasticsearchConfig {
                    auth: ElasticsearchAuth::ApiKey,
                    credential: Some(previous),
                    ..Default::default()
                },
            ),
            Profile::ssh(
                "SSH",
                SshConfig {
                    auth: AuthMethod::Password {
                        credential: Some(previous),
                    },
                    ..Default::default()
                },
            ),
            Profile::redis(
                "Redis",
                rustterm::config::RedisConfig {
                    credential: Some(previous),
                    ..Default::default()
                },
            ),
            Profile::rdp(
                "RDP",
                rustterm::config::RdpConfig {
                    credential: Some(previous),
                    ..Default::default()
                },
            ),
            Profile::mysql(
                "MySQL",
                SqlConfig {
                    credential: Some(previous),
                    ..SqlConfig::mysql()
                },
            ),
            Profile::postgres(
                "PostgreSQL",
                SqlConfig {
                    credential: Some(previous),
                    ..SqlConfig::postgres()
                },
            ),
        ];
        for original in originals {
            for (order, overlap) in [([0, 1], true), ([1, 0], true), ([0, 1], false)] {
                let references = [CredentialRef::new(), CredentialRef::new()];
                let mut profiles = vec![original.clone()];
                let mut pending = HashMap::from([(original.id, references[0])]);
                if overlap {
                    pending.insert(original.id, references[1]);
                }
                let mut unused = Vec::new();
                for index in order {
                    if index == 1 && !overlap {
                        // The second dialog was opened before the first save
                        // finished, so its original profile still has the old ref.
                        pending.insert(original.id, references[1]);
                    }
                    let accepted = complete_credential_write(
                        &mut pending,
                        &mut profiles,
                        &original,
                        references[index],
                        None,
                        Ok(()),
                    )
                    .unwrap();
                    assert_eq!(accepted, index == 1 || !overlap);
                    if !accepted {
                        unused.push(references[index]);
                    }
                }
                assert_eq!(profile_reference(&profiles[0]), Some(references[1]));
                assert_eq!(unused, if overlap { vec![references[0]] } else { vec![] });
                assert!(pending.is_empty());
            }
        }
    }

    #[test]
    fn superseded_credential_errors_cannot_replace_the_latest_outcome() {
        let original = Profile::ssh("SSH", SshConfig::default());
        let older = CredentialRef::new();
        let latest = CredentialRef::new();
        for order in [[0, 1], [1, 0]] {
            let mut profiles = vec![original.clone()];
            let mut pending = HashMap::from([(original.id, latest)]);
            for index in order {
                let (reference, result) = if index == 0 {
                    (older, Err(CredentialError::Unavailable))
                } else {
                    (latest, Ok(()))
                };
                assert_eq!(
                    complete_credential_write(
                        &mut pending,
                        &mut profiles,
                        &original,
                        reference,
                        None,
                        result,
                    )
                    .unwrap(),
                    index == 1
                );
            }
            assert_eq!(profile_reference(&profiles[0]), Some(latest));
        }

        let mut profiles = vec![original.clone()];
        let mut pending = HashMap::from([(original.id, latest)]);
        assert!(matches!(
            complete_credential_write(
                &mut pending,
                &mut profiles,
                &original,
                latest,
                None,
                Err(CredentialError::OutcomeUnknown),
            ),
            Err(CredentialError::OutcomeUnknown)
        ));
        assert!(
            !complete_credential_write(
                &mut pending,
                &mut profiles,
                &original,
                older,
                None,
                Ok(()),
            )
            .unwrap()
        );
        assert_eq!(profiles[0].connection, original.connection);
    }

    #[test]
    fn latest_credential_write_still_requires_the_original_identity_and_route() {
        let original = Profile::ssh("SSH", SshConfig::default());
        let mut changed = original.clone();
        let Connection::Ssh(ssh) = &mut changed.connection else {
            unreachable!()
        };
        ssh.host = "other.example".into();
        let reference = CredentialRef::new();
        let mut pending = HashMap::from([(original.id, reference)]);
        let mut profiles = vec![changed.clone()];
        assert!(
            !complete_credential_write(
                &mut pending,
                &mut profiles,
                &original,
                reference,
                None,
                Ok(()),
            )
            .unwrap()
        );
        assert_eq!(profiles[0].connection, changed.connection);

        let redis = Profile::redis(
            "Private Redis",
            rustterm::config::RedisConfig {
                ssh_profile: Some(original.id),
                ..Default::default()
            },
        );
        let route = (original.id, original.connection.clone());
        let mut pending = HashMap::from([(redis.id, reference)]);
        let mut profiles = vec![redis.clone(), changed];
        assert!(
            !complete_credential_write(
                &mut pending,
                &mut profiles,
                &redis,
                reference,
                Some(&route),
                Ok(()),
            )
            .unwrap()
        );
        assert_eq!(profiles[0].connection, redis.connection);
    }

    #[test]
    fn rdp_trust_and_credential_writes_preserve_each_other() {
        let original = Profile::rdp("Desktop", rustterm::config::RdpConfig::default());
        let fingerprint = "ab".repeat(32);
        let reference = CredentialRef::new();
        let mut current = original.clone();
        assert!(trust_rdp_certificate(&original, &mut current, &fingerprint));
        assert!(attach_credential(
            &mut current,
            &original.connection,
            reference
        ));
        let Connection::Rdp(config) = &current.connection else {
            unreachable!()
        };
        assert_eq!(config.credential, Some(reference));
        assert_eq!(
            config.certificate_sha256.as_deref(),
            Some(fingerprint.as_str())
        );

        let mut current = original.clone();
        assert!(attach_credential(
            &mut current,
            &original.connection,
            reference
        ));
        assert!(trust_rdp_certificate(&original, &mut current, &fingerprint));
        assert_eq!(profile_reference(&current), Some(reference));
    }

    #[test]
    fn rdp_trust_rejects_changed_server_or_newer_trust() {
        let original = Profile::rdp("Desktop", rustterm::config::RdpConfig::default());
        let mut current = original.clone();
        let Connection::Rdp(config) = &mut current.connection else {
            unreachable!()
        };
        config.host = "other-server".into();
        assert!(!trust_rdp_certificate(
            &original,
            &mut current,
            &"ab".repeat(32)
        ));
        assert!(!attach_credential(
            &mut current,
            &original.connection,
            CredentialRef::new()
        ));

        let mut current = original.clone();
        assert!(trust_rdp_certificate(
            &original,
            &mut current,
            &"ab".repeat(32)
        ));
        assert!(!trust_rdp_certificate(
            &original,
            &mut current,
            &"cd".repeat(32)
        ));
    }

    #[test]
    fn saving_credentials_requires_the_original_gateway_identity() {
        let mut gateway = Profile::ssh("Gateway", SshConfig::default());
        let original = (gateway.id, gateway.connection.clone());
        assert!(route_matches(Some(&original), &[gateway.clone()]));
        let Connection::Ssh(ssh) = &mut gateway.connection else {
            unreachable!()
        };
        ssh.auth = AuthMethod::Password {
            credential: Some(CredentialRef::new()),
        };
        assert!(route_matches(Some(&original), &[gateway.clone()]));
        let Connection::Ssh(ssh) = &mut gateway.connection else {
            unreachable!()
        };
        ssh.username = "other-user".into();
        assert!(!route_matches(Some(&original), &[gateway]));
        assert!(!route_matches(Some(&original), &[]));
        assert!(route_matches(None, &[]));
    }
}
