use super::file_clipboard::export::{Export, ExportError};
use super::*;
use rustterm::i18n::messages as i18n_key;
use std::path::PathBuf;
use tempfile::TempDir;

pub(super) struct ContentPublication {
    cancel: tokio_util::sync::CancellationToken,
    _task: Task<()>,
}
impl Drop for ContentPublication {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(super) struct Transfer {
    pub offer_id: u64,
    pub files: usize,
    pub received: u64,
    pub total: u64,
    pub ready: bool,
    publishing: bool,
    export: Option<Arc<Export>>,
    task: Option<Task<()>>,
    ownership: Option<Task<()>>,
}

impl Transfer {
    pub fn text(&self) -> String {
        if self.ready {
            rustterm::trf!(i18n_key::RDP_RECEIVED_FILE_COUNT, files = self.files)
        } else {
            rustterm::trf!(
                i18n_key::RDP_RECEIVING_FILE_COUNT,
                files = self.files,
                received = crate::app::file_transfer::format_bytes(self.received),
                total = crate::app::file_transfer::format_bytes(self.total)
            )
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.ready
            && !self.publishing
            && let Some(export) = &self.export
        {
            export.cancel();
        }
    }
}

// URI selections have no destination-copy completion event. Retain published
// sources until application exit so changing the clipboard or closing a pane
// cannot delete a source while a file manager is still copying it.
#[derive(Default)]
struct FileClipboard {
    current: Option<Arc<Export>>,
    sources: Vec<Arc<TempDir>>,
}
impl gpui::Global for FileClipboard {}

impl RdpView {
    pub(super) fn cancel_remote_file_receive(&mut self, cx: &mut Context<Self>) {
        self.file_export_snapshot = None;
        self.content_publication = None;
        if let Some(transfer) = self.incoming_files.take()
            && !transfer.ready
        {
            if let Some(export) = &transfer.export {
                export.cancel();
            }
            self.send(
                RdpCommand::CancelReceiveFiles {
                    offer_id: transfer.offer_id,
                },
                cx,
            );
        }
    }

    pub(super) fn publish_remote_content(
        &mut self,
        revision: u64,
        content: Arc<rustrdp::ClipboardContent>,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked()
            || revision != self.clipboard.revision()
            || !self.config.clipboard
            || self.closed
            || self.state != State::Connected
        {
            return;
        }
        let Some(snapshot) = self.file_export_snapshot.take() else {
            return;
        };
        self.clipboard.cancel_export();
        self.content_publication = None;
        if !cx.has_global::<FileClipboard>() {
            cx.set_global(FileClipboard::default());
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let text = content.text.clone();
        let task = cx.spawn(async move |this, cx| {
            let snapshot = snapshot.await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    let formats =
                        file_clipboard::to_mime(&content).map_err(|_| ExportError::Invalid)?;
                    if worker_cancel.is_cancelled() {
                        return Err(ExportError::Changed);
                    }
                    let export = Arc::new(Export::begin(snapshot?)?);
                    if worker_cancel.is_cancelled() {
                        export.cancel();
                        return Err(ExportError::Changed);
                    }
                    export.publish_content(formats)?;
                    if worker_cancel.is_cancelled() {
                        export.cancel();
                        return Err(ExportError::Changed);
                    }
                    Ok(export)
                })
                .await;
            if let Ok(export) = &result {
                let current = export.clone();
                let _ = cx.update(|cx| {
                    if current.is_current() {
                        cx.global_mut::<FileClipboard>().current = Some(current);
                    }
                });
            }
            let _ = this.update(cx, |this, cx| {
                if revision != this.clipboard.revision()
                    || this.window_state.is_locked()
                    || this.closed
                {
                    return;
                }
                match result {
                    Ok(export) if export.is_current() => {
                        this.clipboard.remote_content(export.cancellation(), text);
                        this.clipboard.feedback = None;
                        this.window_state
                            .record("rdp-clipboard-content-received", this.index);
                    }
                    Err(ExportError::Changed) => {}
                    _ => {
                        this.clipboard.feedback = Some(clipboard::Feedback::Rejected(
                            rustterm::rdp::RdpError::ClipboardUnavailable,
                        ))
                    }
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        });
        self.content_publication = Some(ContentPublication {
            cancel,
            _task: task,
        });
    }

    pub(super) fn receive_remote_files(
        &mut self,
        revision: u64,
        offer_id: u64,
        files: usize,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked()
            || revision != self.clipboard.revision()
            || !self.config.clipboard
            || !self.config.clipboard_files
            || self.closed
            || self.state != State::Connected
        {
            self.send(RdpCommand::CancelReceiveFiles { offer_id }, cx);
            return;
        }
        let Some(snapshot) = self.file_export_snapshot.take() else {
            self.send(RdpCommand::CancelReceiveFiles { offer_id }, cx);
            return;
        };
        // Eligibility was captured while the remote copy was active. The native
        // snapshot guards all later local copies, even after Alt-Tab when GPUI
        // cannot reliably read a Wayland selection's text.
        self.clipboard.cancel_export();
        self.incoming_files = Some(Transfer {
            offer_id,
            files,
            received: 0,
            total,
            ready: false,
            publishing: false,
            export: None,
            task: None,
            ownership: None,
        });
        self.error = None;
        self.notice = None;
        self.file_transfer = None;
        self.clipboard.feedback = None;
        let task = cx.spawn(async move |this, cx| {
            let result = match snapshot.await {
                Ok(snapshot) => {
                    cx.background_executor()
                        .spawn(async move { Export::begin(snapshot).map(Arc::new) })
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |this, cx| {
                if this.window_state.is_locked()
                    || this
                        .incoming_files
                        .as_ref()
                        .is_none_or(|transfer| transfer.offer_id != offer_id)
                    || this.clipboard.revision() != revision
                    || this.closed
                {
                    return;
                }
                let export = match result {
                    Ok(export) if export.is_current() => export,
                    result => {
                        this.remote_file_error(
                            offer_id,
                            result.err().unwrap_or(ExportError::Changed),
                            cx,
                        );
                        return;
                    }
                };
                let cancelled = export.cancellation();
                let ownership = cx.spawn(async move |this, cx| {
                    cancelled.cancelled().await;
                    let _ = this.update(cx, |this, cx| {
                        if this
                            .incoming_files
                            .as_ref()
                            .is_some_and(|transfer| transfer.offer_id == offer_id)
                        {
                            let pending = this
                                .incoming_files
                                .as_ref()
                                .is_some_and(|transfer| !transfer.ready);
                            this.cancel_remote_file_receive(cx);
                            if pending {
                                this.notice = Some(
                                    i18n_key::RDP_THE_LOCAL_CLIPBOARD_CHANGED_COPY_THE_REMOTE_FILES_AGAIN,
                                );
                            }
                            cx.emit(PaneEvent::Activity);
                            cx.notify();
                        }
                    });
                });
                let transfer = this
                    .incoming_files
                    .as_mut()
                    .expect("current offer was checked");
                transfer.export = Some(export);
                transfer.ownership = Some(ownership);
                this.send(RdpCommand::ReceiveFiles { offer_id }, cx);
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        });
        self.incoming_files
            .as_mut()
            .expect("offer was installed")
            .task = Some(task);
    }

    pub(super) fn publish_remote_files(
        &mut self,
        offer_id: u64,
        directory: Arc<TempDir>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked() {
            self.cancel_remote_file_receive(cx);
            return;
        }
        let Some(transfer) = self
            .incoming_files
            .as_mut()
            .filter(|transfer| transfer.offer_id == offer_id)
        else {
            return;
        };
        let Some(export) = transfer.export.clone().filter(|export| export.is_current()) else {
            self.remote_file_error(offer_id, ExportError::Changed, cx);
            return;
        };
        transfer.received = transfer.total;
        transfer.publishing = true;
        transfer.task = None;
        if !cx.has_global::<FileClipboard>() {
            cx.set_global(FileClipboard::default());
        }
        let publishing = export.clone();
        let retained = directory.clone();
        let pending = cx
            .background_executor()
            .spawn(async move { publishing.publish(directory, paths) });
        cx.spawn(async move |this, cx| {
            let result = pending.await;
            // Clipboard lifetime belongs to the application, independently of
            // the initiating pane's lifetime or its connection state.
            if result.is_ok() {
                let current = export.clone();
                let _ = cx.update(|cx| {
                    // A successful native publication may already have handed
                    // the URI to a file manager, even if ownership changed or
                    // the originating pane closed before this callback ran.
                    cx.global_mut::<FileClipboard>().sources.push(retained);
                    if current.is_current() {
                        cx.global_mut::<FileClipboard>().current = Some(current);
                    }
                });
            } else {
                cx.background_executor()
                    .spawn(async move { drop(retained) })
                    .detach();
            }
            let _ = this.update(cx, |this, cx| {
                let Some(transfer) = this
                    .incoming_files
                    .as_mut()
                    .filter(|transfer| transfer.offer_id == offer_id)
                else {
                    return;
                };
                match result {
                    Ok(()) if export.is_current() => {
                        transfer.ready = true;
                        this.clipboard.remote_files(export.cancellation());
                    }
                    result => this.remote_file_error(
                        offer_id,
                        result.err().unwrap_or(ExportError::Changed),
                        cx,
                    ),
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        })
        .detach();
    }

    fn remote_file_error(&mut self, offer_id: u64, error: ExportError, cx: &mut Context<Self>) {
        if self
            .incoming_files
            .as_ref()
            .is_some_and(|transfer| transfer.offer_id == offer_id)
        {
            self.cancel_remote_file_receive(cx);
            self.error = Some(Message::new(error.message()));
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }
}
