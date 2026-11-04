use gpui::{Context, Task, Timer};
use rustterm::{
    config::Settings,
    i18n::{Message, messages as key},
    update::{self, Installation, Progress, Release, UpdateControl},
};
use std::{sync::Arc, time::Duration};

#[derive(Default)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Current,
    Available,
    Downloading(u64, u64),
    Verifying,
    Installing,
    Installed,
}

pub struct Updates {
    pub status: Status,
    pub error: Option<Message>,
    pub release: Option<Release>,
    pub installation: Option<Installation>,
    automatic: bool,
    proxy: String,
    cancel: Arc<UpdateControl>,
    _schedule: Task<()>,
    operation: Option<Task<()>>,
}
impl Updates {
    pub fn new(settings: &Settings, cx: &mut Context<Self>) -> Self {
        let schedule = cx.spawn(async move |this, cx| {
            // Leave startup/credential restoration responsive and do not open a modal.
            Timer::after(Duration::from_secs(5)).await;
            loop {
                if this
                    .update(cx, |this, cx| {
                        if this.automatic
                            && !this.busy()
                            && !matches!(this.status, Status::Installed)
                        {
                            this.check(this.proxy.clone(), cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                Timer::after(Duration::from_secs(24 * 60 * 60)).await;
            }
        });
        Self {
            status: Status::Idle,
            error: None,
            release: None,
            installation: Installation::current().ok(),
            automatic: settings.check_updates,
            proxy: settings.update_proxy.clone(),
            cancel: Arc::new(UpdateControl::default()),
            _schedule: schedule,
            operation: None,
        }
    }

    pub fn configure(&mut self, settings: &Settings, cx: &mut Context<Self>) {
        let changed =
            (!self.automatic && settings.check_updates) || self.proxy != settings.update_proxy;
        self.automatic = settings.check_updates;
        self.proxy.clone_from(&settings.update_proxy);
        if changed && self.automatic {
            self.check(self.proxy.clone(), cx);
        }
        cx.notify();
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Checking) || self.updating()
    }
    pub fn updating(&self) -> bool {
        matches!(
            self.status,
            Status::Downloading(..) | Status::Verifying | Status::Installing
        )
    }
    pub fn can_cancel(&self) -> bool {
        matches!(self.status, Status::Downloading(..) | Status::Verifying)
            && self.cancel.can_cancel()
    }
    pub fn prepare_exit(&self) -> bool {
        !self.updating() || self.cancel.cancel()
    }
    pub fn attention(&self) -> bool {
        self.release.is_some() || self.updating()
    }
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.can_cancel() {
            self.cancel.cancel();
            cx.notify();
        }
    }
    pub fn message(&self) -> Message {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let version = self
            .release
            .as_ref()
            .map_or("", |release| release.version.as_str());
        if self.updating()
            && !matches!(self.status, Status::Installing)
            && self.cancel.is_cancelled()
        {
            return key::UPDATES_CANCELLING.into();
        }
        match self.status {
            Status::Idle => key::UPDATES_IDLE.into(),
            Status::Checking => key::UPDATES_CHECKING.into(),
            Status::Current => key::UPDATES_CURRENT.into(),
            Status::Available => rustterm::msg!(key::UPDATES_AVAILABLE, version = version),
            Status::Downloading(received, total) => rustterm::msg!(
                key::UPDATES_DOWNLOADING,
                percent = received.saturating_mul(100) / total.max(1)
            ),
            Status::Verifying => key::UPDATES_VERIFYING.into(),
            Status::Installing => key::UPDATES_INSTALLING.into(),
            Status::Installed => rustterm::msg!(key::UPDATES_INSTALLED, version = version),
        }
    }

    pub fn check(&mut self, proxy: String, cx: &mut Context<Self>) {
        if self.busy() || matches!(self.status, Status::Installed) {
            return;
        }
        self.status = Status::Checking;
        self.error = None;
        self.release = None;
        let worker = cx
            .background_executor()
            .spawn(async move { update::check(&proxy) });
        self.operation = Some(cx.spawn(async move |this, cx| {
            let result = worker.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(release) => {
                        this.status = if release.is_some() {
                            Status::Available
                        } else {
                            Status::Current
                        };
                        this.release = release;
                    }
                    Err(error) => {
                        this.status = Status::Idle;
                        this.error = Some(rustterm::msg!(
                            key::UPDATES_FAILED,
                            error = Message::from_error(error)
                        ));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn install(&mut self, proxy: String, cx: &mut Context<Self>) {
        if self.busy() || matches!(self.status, Status::Installed) {
            return;
        }
        let (Some(release), Some(installation)) = (self.release.clone(), self.installation.clone())
        else {
            return;
        };
        if let Err(error) = update::validate_proxy(&proxy) {
            self.error = Some(Message::from_error(error));
            cx.notify();
            return;
        }
        self.cancel = Arc::new(UpdateControl::default());
        let cancel = self.cancel.clone();
        let (send, mut receive) = tokio::sync::mpsc::channel(8);
        let mut worker = cx.background_executor().spawn(async move {
            update::download_and_install(&release, &proxy, &installation, &cancel, |progress| {
                let _ = send.try_send(progress);
            })
        });
        self.status = Status::Downloading(0, 0);
        self.error = None;
        self.operation = Some(cx.spawn(async move |this, cx| {
            loop {
                tokio::select! {
                    result = &mut worker => {
                        let _ = this.update(cx, |this, cx| {
                            match result {
                                Ok(()) => this.status = Status::Installed,
                                Err(error) => {
                                    this.status = Status::Available;
                                    this.error = Some(if this.cancel.is_cancelled() {
                                        key::UPDATES_CANCELLED.into()
                                    } else { rustterm::msg!(key::UPDATES_FAILED, error = Message::from_error(error)) });
                                }
                            }
                            cx.notify();
                        });
                        return;
                    }
                    Some(progress) = receive.recv() => {
                        let _ = this.update(cx, |this, cx| {
                            this.status = match progress {
                                Progress::Download { received, total } => Status::Downloading(received, total),
                                Progress::Verifying => Status::Verifying,
                                Progress::Installing => Status::Installing,
                            };
                            cx.notify();
                        });
                    }
                }
            }
        }));
        cx.notify();
    }
}
impl Drop for Updates {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
