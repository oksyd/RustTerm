use super::connections;
use super::{Modal, Workspace};
use crate::app::ui;
use gpui::{Context, PromptLevel, Window};
use rustterm::config::CredentialStore;
use rustterm::config::{Connection, CredentialRef, ProcessCredentialStore, Secret};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};
use std::time::Duration;

use gpui::{Task, Timer};
use rustterm::{
    connection_probe::{self, ProbeOutcome, ProbeRequest},
    rdp::{CertificateDecision, RdpEvent},
    session::HostKeyChallenge,
};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(super) struct ProfileTest {
    id: usize,
    status: TestStatus,
    cancel: CancellationToken,
    _task: Task<()>,
}

enum TestStatus {
    Running,
    Success(ProbeOutcome, Duration),
    Failed(Message),
}

impl ProfileTest {
    pub(super) fn is_running(&self) -> bool {
        matches!(self.status, TestStatus::Running)
    }

    pub(super) fn color(&self) -> u32 {
        match self.status {
            TestStatus::Running => ui::MUTED,
            TestStatus::Success(..) => ui::SUCCESS,
            TestStatus::Failed(_) => ui::DANGER,
        }
    }

    pub(super) fn message(&self) -> String {
        match &self.status {
            TestStatus::Running => tr(i18n_key::DATABASE_TESTING_CONNECTION).into(),
            TestStatus::Failed(error) => error.render(),
            TestStatus::Success(outcome, elapsed) => rustterm::trf!(
                i18n_key::CONNECTIONS_RESULT_ELAPSED_MS,
                result = tr(match outcome {
                    ProbeOutcome::Authenticated =>
                        i18n_key::CONNECTIONS_SSH_HOST_VERIFICATION_AND_AUTHENTICATION_SUCCEEDED,
                    ProbeOutcome::DatabaseReady => i18n_key::CONNECTIONS_DATABASE_AUTHENTICATION_AND_QUERY_SUCCEEDED,
                    ProbeOutcome::SqliteReady => i18n_key::DATABASE_DATABASE_ACCESS_CHECK_SUCCEEDED,
                    ProbeOutcome::TelnetReachable =>
                        i18n_key::CONNECTIONS_TELNET_PORT_IS_REACHABLE_LOGIN_WAS_NOT_TESTED,
                    ProbeOutcome::RdpAuthenticated =>
                        i18n_key::CONNECTIONS_RDP_TLS_AND_NLA_AUTHENTICATION_SUCCEEDED_DESKTOP_ACTIVATION_WAS_NOT,
                }),
                elapsed = elapsed.as_millis(),
            ),
        }
    }
}

impl Drop for ProfileTest {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Workspace {
    pub(super) fn cancel_profile_test(&mut self) {
        self.profile_test = None;
    }

    fn profile_test_current(&self, id: usize) -> bool {
        !self.closing
            && !self.window_state.is_locked()
            && matches!(self.modal, Some(Modal::Profile(_)))
            && self
                .profile_test
                .as_ref()
                .is_some_and(|test| test.id == id && test.is_running())
    }

    pub(super) fn test_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing
            || self.modal_saving
            || self.window_state.is_locked()
            || self.window_state.prompts.is_busy()
            || self.modal_composing(cx)
            || self.authentication.read(cx).is_active()
        {
            return;
        }
        if self
            .profile_test
            .as_ref()
            .is_some_and(ProfileTest::is_running)
        {
            self.cancel_profile_test();
            cx.notify();
            return;
        }
        let Some(Modal::Profile(form)) = &self.modal else {
            return;
        };
        let prepared = (|| {
            let profile = form.read(cx).profile(&self.config.profiles, cx)?;
            let credentials = form.read(cx).secret_for_test(&self.config.profiles, cx)?;
            let tunnel = profile.connection.ssh_profile().map(|id| {
                self.config.profiles.iter()
                    .find(|saved| saved.id == id && matches!(saved.connection, Connection::Ssh(_)))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!(tr(i18n_key::REDIS_THE_SELECTED_SSH_PROFILE_IS_NO_LONGER_AVAILABLE_CHOOSE_ANOTHER)))
            }).transpose()?;
            Ok::<_, anyhow::Error>((profile, credentials, tunnel))
        })();
        let (mut profile, credentials, tunnel) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let shown = form.update(cx, |form, cx| {
                    form.reveal_validation_error(&error, window, cx)
                });
                self.error = (!shown).then(|| Message::from_error(error));
                cx.notify();
                return;
            }
        };
        if matches!(profile.connection, Connection::Local(_)) {
            return;
        }
        if credentials.1.is_some() {
            profile.connection.set_credential(None);
        }
        self.cancel_profile_test();
        self.error = None;
        let id = self.id();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let known_hosts = self.path.join("known_hosts.toml");
        let authentication = self.window_state.authentication.clone();
        let (host_keys, mut hosts) = mpsc::channel(1);
        let (certificates, mut certificate_events) = mpsc::channel(4);
        let started = Instant::now();
        let mut test = cx.background_executor().spawn(async move {
            let secret = resolve_secret(credentials)?;
            if connections::needs_password(&profile) && secret.expose().is_empty() {
                return Err(i18n_key::WORKSPACE_ENTER_A_PASSWORD_NO_AUTHENTICATION_REQUEST_HAS_BEEN_SENT.into());
            }
            let mut ssh = if let Connection::Ssh(config) = &profile.connection {
                Some(connections::ssh_options(config, &secret, known_hosts))
            } else if let Some(tunnel) = tunnel {
                let secret = resolve_secret((tunnel.connection.credential_ref(), None))?;
                if connections::needs_password(&tunnel) && secret.expose().is_empty() {
                    return Err(i18n_key::CONNECTIONS_SAVE_A_PASSWORD_IN_THE_SSH_TUNNEL_PROFILE_BEFORE_TESTING.into());
                }
                let Connection::Ssh(config) = &tunnel.connection else { unreachable!("validated SSH route") };
                Some(connections::ssh_options(config, &secret, known_hosts))
            } else { None };
            if let Some(options) = &mut ssh {
                options.authentication = Some(authentication);
            }
            connection_probe::test(
                ProbeRequest { profile, secret, sentinel_secret: None, ssh },
                host_keys,
                certificates,
                worker_cancel,
            ).await.map_err(Message::from_error)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                tokio::select! {
                    result = &mut test => {
                        let _ = this.update(cx, |this, cx| {
                            if !this.profile_test_current(id) { return; }
                            if let Some(test) = &mut this.profile_test {
                                test.status = match result {
                                    Ok(outcome) => TestStatus::Success(outcome, started.elapsed()),
                                    Err(error) => TestStatus::Failed(error),
                                };
                            }
                            cx.notify();
                        });
                        return;
                    },
                    Some(challenge) = hosts.recv() => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.prompt_probe_trust(id, ProbeTrust::host(challenge), window, cx);
                        });
                    },
                    Some(event) = certificate_events.recv() => {
                        if let RdpEvent::CertificateChallenge { host, port, fingerprint, previous_fingerprint, reply } = event {
                            let detail = match previous_fingerprint {
                                Some(previous) => rustterm::msg!(i18n_key::RDP_THE_RDP_SERVER_CERTIFICATE_HAS_CHANGED_SERVER_HOST_PORT_SAVED, host=host, port=port, previous=previous, fingerprint=fingerprint),
                                None => rustterm::msg!(i18n_key::RDP_SERVER_HOST_PORT_CERTIFICATE_SHA_256_FINGERPRINT_VERIFY_THIS_FINGERPRINT, host=host, port=port, fingerprint=fingerprint),
                            };
                            let _ = this.update_in(cx, |this, window, cx| {
                                this.prompt_probe_trust(id, ProbeTrust { title: i18n_key::CONNECTIONS_VERIFY_RDP_CERTIFICATE, detail, accept: i18n_key::RDP_TRUST_ONCE, reply: TrustReply::Rdp(reply) }, window, cx);
                            });
                        }
                    },
                }
            }
        });
        self.profile_test = Some(ProfileTest {
            id,
            status: TestStatus::Running,
            cancel,
            _task: task,
        });
        cx.notify();
    }

    fn prompt_probe_trust(
        &mut self,
        id: usize,
        challenge: ProbeTrust,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompts = self.window_state.prompts.clone();
        // Keep native prompts and their permit alive until answered. Cancelling the
        // test invalidates the answer without allowing another prompt to replace it.
        cx.spawn_in(window, async move |this, cx| {
            let permit = loop {
                if challenge.reply.is_closed() {
                    return;
                }
                let state = this
                    .update(cx, |this, _| {
                        this.profile_test_current(id).then(|| {
                            this.window_state
                                .visibility
                                .is_visible()
                                .then(|| prompts.acquire())
                                .flatten()
                        })
                    })
                    .ok()
                    .flatten();
                match state {
                    Some(Some(permit)) => break permit,
                    Some(None) => {
                        Timer::after(Duration::from_millis(100)).await;
                    }
                    None => return,
                }
            };
            let response = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    tr(challenge.title),
                    Some(&challenge.detail.render()),
                    &[tr(i18n_key::COMMON_CANCEL), tr(challenge.accept)],
                    cx,
                )
            });
            let accepted = match response {
                Ok(response) => response.await == Ok(1),
                Err(_) => false,
            };
            let current = this
                .update(cx, |this, _| this.profile_test_current(id))
                .unwrap_or(false);
            challenge
                .reply
                .respond(accepted && current && permit.is_valid());
        })
        .detach();
    }
}

fn resolve_secret(
    (reference, provided): (Option<CredentialRef>, Option<Secret>),
) -> Result<Secret, Message> {
    match (reference, provided) {
        (_, Some(secret)) => Ok(secret),
        (Some(reference), None) => ProcessCredentialStore::new()
            .get(reference)
            .map_err(Message::from_error),
        (None, None) => Ok(Secret::new(String::new())),
    }
}

struct ProbeTrust {
    title: &'static str,
    detail: Message,
    accept: &'static str,
    reply: TrustReply,
}
impl ProbeTrust {
    fn host(challenge: HostKeyChallenge) -> Self {
        Self {
            title: i18n_key::CONNECTIONS_VERIFY_SSH_HOST_KEY,
            detail: rustterm::msg!(
                i18n_key::CONNECTIONS_SSH_FINGERPRINT_CONFIRMATION,
                host = challenge.host,
                port = challenge.port,
                algorithm = challenge.algorithm,
                fingerprint = challenge.fingerprint
            ),
            accept: i18n_key::TERMINAL_TRUST_AND_SAVE,
            reply: TrustReply::Ssh(challenge.reply),
        }
    }
}
enum TrustReply {
    Ssh(oneshot::Sender<bool>),
    Rdp(oneshot::Sender<CertificateDecision>),
}
impl TrustReply {
    fn is_closed(&self) -> bool {
        match self {
            Self::Ssh(reply) => reply.is_closed(),
            Self::Rdp(reply) => reply.is_closed(),
        }
    }
    fn respond(self, accepted: bool) {
        match self {
            Self::Ssh(reply) => {
                let _ = reply.send(accepted);
            }
            Self::Rdp(reply) => {
                let _ = reply.send(if accepted {
                    CertificateDecision::Once
                } else {
                    CertificateDecision::Reject
                });
            }
        }
    }
}
