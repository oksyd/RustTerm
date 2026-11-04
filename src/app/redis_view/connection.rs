use super::*;
use crate::app::interaction::PromptPermit;
use gpui::Timer;
use rustterm::i18n::messages as i18n_key;
use rustterm::session::HostKeyChallenge;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

impl RedisView {
    pub(super) fn connect(
        &mut self,
        secret: Option<Secret>,
        sentinel_secret: Option<Secret>,
        ssh: Option<SshOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = self.config.clone();
        self.busy = Some(i18n_key::COMMON_CONNECTING_PROGRESS);
        let generation = self.generation;
        let tunnel = ssh.map(|ssh| (ssh, self.trust_channel(window, cx)));
        let task = cx.background_executor().spawn(async move {
            RedisClient::connect_authenticated(config, secret, sentinel_secret, tunnel).await
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || generation != this.generation {
                    if let Ok(client) = result {
                        client.close();
                    }
                    return;
                }
                this.trust_task = None;
                this.busy = None;
                match result {
                    Ok(client) => {
                        this.watch_connection(client.clone(), cx);
                        this.client = Some(client);
                        this.state = ConnectionState::Connected;
                        this.manual_disconnect = false;
                        this.retryable_failure = true;
                        this.reload(this.selected_key.clone(), cx);
                    }
                    Err(error) => {
                        this.state = ConnectionState::Failed;
                        this.fail(error, cx);
                    }
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
    }

    fn trust_channel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> mpsc::Sender<HostKeyChallenge> {
        let (sender, mut receiver) = mpsc::channel::<HostKeyChallenge>(1);
        let generation = self.generation;
        let prompts = self.window_state.prompts.clone();
        self.trust_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(challenge) = receiver.recv().await {
                loop {
                    if challenge.reply.is_closed() {
                        return;
                    }
                    let current = this
                        .update(cx, |this, cx| {
                            if !this.awaiting_host_key(generation) {
                                return None;
                            }
                            if this.state != ConnectionState::WaitingHostKey {
                                this.state = ConnectionState::WaitingHostKey;
                                this.busy = Some(i18n_key::TERMINAL_WAITING_FOR_HOST_CONFIRMATION);
                                cx.emit(PaneEvent::Activity);
                                cx.notify();
                            }
                            Some(this.visible && this.window_state.visibility.is_visible())
                        })
                        .ok()
                        .flatten();
                    match current {
                        Some(true) => {
                            if let Some(permit) = prompts.acquire() {
                                let done = cx
                                    .update(|window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.prompt_host_key(
                                                challenge, permit, generation, window, cx,
                                            )
                                        })
                                    })
                                    .ok()
                                    .and_then(Result::ok)
                                    .flatten();
                                if let Some(done) = done {
                                    let _ = done.await;
                                }
                                break;
                            }
                        }
                        Some(false) => {}
                        None => return,
                    }
                    // Only wait while an actual SSH handshake needs the exclusive
                    // window prompt. Dropping this task cancels this wait immediately.
                    Timer::after(Duration::from_millis(100)).await;
                }
            }
        }));
        sender
    }

    fn awaiting_host_key(&self, generation: usize) -> bool {
        !self.closed
            && self.generation == generation
            && matches!(
                self.state,
                ConnectionState::Connecting | ConnectionState::WaitingHostKey
            )
    }

    fn prompt_host_key(
        &mut self,
        challenge: HostKeyChallenge,
        permit: PromptPermit,
        generation: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<oneshot::Receiver<()>> {
        if !self.awaiting_host_key(generation) || challenge.reply.is_closed() || !permit.is_valid()
        {
            return None;
        }
        let (finished, done) = oneshot::channel();
        self.native_pending = true;
        let detail = rustterm::trf!(
            i18n_key::CONNECTIONS_SSH_FINGERPRINT_REDIS_TUNNEL,
            host = challenge.host,
            port = challenge.port,
            algorithm = challenge.algorithm,
            fingerprint = challenge.fingerprint,
            target = self.config.host,
            target_port = self.config.port,
        );
        let response = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::REDIS_VERIFY_THE_SSH_TUNNEL_HOST_KEY),
            Some(&detail),
            &[
                tr(i18n_key::COMMON_CANCEL),
                tr(i18n_key::TERMINAL_TRUST_AND_SAVE),
            ],
            cx,
        );
        // GPUI's native prompt has no programmatic dismiss API. Keep its permit
        // alive until the dialog is answered, even if this pane is cancelled.
        // The generation check below prevents a late answer from trusting a new
        // or already cancelled connection.
        cx.spawn_in(window, async move |this, cx| {
            let trusted = response.await == Ok(1);
            let current = this
                .update(cx, |this, cx| {
                    this.native_pending = false;
                    let current = this.awaiting_host_key(generation) && permit.is_valid();
                    if current {
                        this.state = ConnectionState::Connecting;
                        this.busy = Some(i18n_key::COMMON_CONNECTING_PROGRESS);
                        cx.emit(PaneEvent::Activity);
                    }
                    cx.notify();
                    current
                })
                .unwrap_or(false);
            let _ = challenge.reply.send(trusted && current);
            drop(permit);
            let _ = finished.send(());
        })
        .detach();
        cx.notify();
        Some(done)
    }
}
