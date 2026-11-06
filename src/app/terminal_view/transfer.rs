use super::TerminalView;
use crate::app::{
    pane::PaneEvent,
    ui::{self, Icon},
};
use gpui::{AnyElement, Context, PathPromptOptions, PromptLevel, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use rustterm::session::{Session, ZmodemDirection, ZmodemPhase, ZmodemSelection, ZmodemStatus};

impl TerminalView {
    pub(super) fn poll_transfer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let status = self
            .runs
            .get(self.viewed_run)
            .and_then(Session::zmodem_status);
        if self.zmodem != status {
            self.zmodem = status;
            cx.notify();
            cx.emit(PaneEvent::Activity);
        }
        if self.visible
            && self.window_state.visibility.is_visible()
            && self.focus.is_focused(window)
            && let Some(status) = &self.zmodem
            && status.phase == ZmodemPhase::Waiting
            && let Some(run) = self.runs.get(self.viewed_run)
            && self.zmodem_prompt != Some((run.id(), status.id))
        {
            self.choose_transfer_files(window, cx);
        }
    }

    fn choose_transfer_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.runs.get(self.viewed_run) else {
            return;
        };
        let Some(status) = run
            .zmodem_status()
            .filter(|s| s.phase == ZmodemPhase::Waiting)
        else {
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let session_id = run.id();
        let id = status.id;
        let download = status.progress.direction == ZmodemDirection::Download;
        self.zmodem_prompt = Some((session_id, id));
        let response = cx.prompt_for_paths(PathPromptOptions {
            files: !download,
            directories: download,
            multiple: !download,
            prompt: Some(
                if download {
                    tr(i18n_key::TERMINAL_ZMODEM_SELECT_A_DOWNLOAD_FOLDER)
                } else {
                    tr(i18n_key::TERMINAL_ZMODEM_SELECT_FILES_TO_UPLOAD)
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = response.await;
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid();
                drop(permit);
                let Some(run) = this.runs.iter().find(|run| run.id() == session_id) else {
                    return;
                };
                if !valid {
                    let _ = run.cancel_zmodem(id);
                    return;
                }
                let selection = match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => Some(if download {
                        ZmodemSelection::DownloadDirectory(paths.remove(0))
                    } else {
                        ZmodemSelection::UploadFiles(paths)
                    }),
                    Ok(Ok(_)) => None,
                    Ok(Err(error)) => {
                        this.error = Some(rustterm::i18n::Message::with_arg(
                            i18n_key::TERMINAL_FILE_SELECTION_FAILED_ERROR,
                            "error",
                            rustterm::i18n::Message::from_error(error),
                        ));
                        None
                    }
                    Err(_) => {
                        this.error = Some(i18n_key::TERMINAL_THE_FILE_PICKER_WAS_CLOSED.into());
                        None
                    }
                };
                let result = match selection {
                    Some(selection) => run.accept_zmodem(id, selection),
                    None => run.cancel_zmodem(id),
                };
                if let Err(error) = result
                    && !matches!(
                        error,
                        rustterm::session::SendError::NoTransfer
                            | rustterm::session::SendError::Closed
                    )
                {
                    this.error = Some(rustterm::i18n::Message::with_arg(
                        i18n_key::TERMINAL_FILE_TRANSFER_REQUEST_WAS_NOT_ACCEPTED_ERROR,
                        "error",
                        rustterm::i18n::Message::from_error(error),
                    ));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_transfer(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.runs.get(self.viewed_run)
            && let Some(status) = run.zmodem_status()
        {
            if status.is_active() {
                if let Err(error) = run.cancel_zmodem(status.id)
                    && error != rustterm::session::SendError::NoTransfer
                {
                    self.error = Some(rustterm::i18n::Message::with_arg(
                        i18n_key::TERMINAL_COULD_NOT_CANCEL_FILE_TRANSFER_ERROR,
                        "error",
                        rustterm::i18n::Message::from_error(error),
                    ));
                }
            } else {
                run.dismiss_zmodem(status.id);
                self.zmodem = None;
            }
            cx.notify();
        }
    }

    fn transfer_details(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.zmodem else {
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let mut files = status
            .files
            .iter()
            .take(16)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if status.files.len() > 16 {
            files.push_str(&rustterm::trf!(
                i18n_key::SFTP_ADDITIONAL_ITEM_COUNT,
                count = status.files.len() - 16,
            ));
        }
        let mut details = transfer_phase_label(status).to_owned();
        // Retain the event's full outcome, including cleanup errors and paths.
        // The phase label follows language changes; backend details stay intact.
        if !status.message.is_empty() && status.message != details {
            details.push_str("\n\n");
            details.push_str(&status.message);
        }
        if !files.is_empty() {
            details.push_str("\n\n");
            details.push_str(&files);
        }
        let result = window.prompt(
            PromptLevel::Info,
            tr(i18n_key::TERMINAL_FILE_TRANSFER),
            Some(&details),
            &[tr(i18n_key::COMMON_CLOSE)],
            cx,
        );
        cx.spawn_in(window, async move |_, _| {
            let _ = result.await;
            drop(permit);
        })
        .detach();
    }

    pub(super) fn render_transfer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.zmodem.as_ref()?;
        let direction = if status.progress.direction == ZmodemDirection::Download {
            tr(i18n_key::TERMINAL_DOWNLOAD)
        } else {
            tr(i18n_key::TERMINAL_UPLOAD)
        };
        let progress = if let Some(total) = status.progress.total_bytes {
            format!("{} / {} B", status.progress.bytes_transferred, total)
        } else {
            format!("{} B", status.progress.bytes_transferred)
        };
        let label = rustterm::trf!(
            i18n_key::TERMINAL_ZMODEM_DIRECTION_STATUS,
            direction = direction,
            status = if status.phase == ZmodemPhase::Running {
                progress.as_str()
            } else {
                transfer_phase_label(status)
            },
        );
        Some(
            div()
                .h(px(28.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(8.))
                .bg(rgb(ui::colors().raised))
                .border_b_1()
                .border_color(rgb(ui::colors().border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(rgb(if status.phase == ZmodemPhase::Failed {
                            ui::colors().danger
                        } else {
                            ui::colors().accent
                        }))
                        .child(label),
                )
                .when(status.phase == ZmodemPhase::Waiting, |d| {
                    d.child(
                        ui::text_button(
                            ("transfer-choose", self.index),
                            i18n_key::TERMINAL_CHOOSE,
                            ui::ButtonStyle::Primary,
                        )
                        .text_size(px(10.))
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.choose_transfer_files(window, cx)
                            }),
                        ),
                    )
                })
                .child(
                    ui::icon_button(
                        ("transfer-details", self.index),
                        Icon::More,
                        i18n_key::TERMINAL_VIEW_TRANSFER_DETAILS,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.transfer_details(window, cx))),
                )
                .child(
                    ui::icon_button(
                        ("transfer-cancel", self.index),
                        Icon::Close,
                        if status.is_active() {
                            i18n_key::TERMINAL_CANCEL_TRANSFER
                        } else {
                            i18n_key::TERMINAL_DISMISS_TRANSFER_RESULT
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_transfer(cx))),
                )
                .into_any_element(),
        )
    }
}

fn transfer_phase_label(status: &ZmodemStatus) -> &'static str {
    match status.phase {
        ZmodemPhase::Waiting => match status.progress.direction {
            ZmodemDirection::Download => tr(i18n_key::TERMINAL_CHOOSE_A_DOWNLOAD_FOLDER),
            ZmodemDirection::Upload => tr(i18n_key::TERMINAL_CHOOSE_FILES_TO_UPLOAD),
        },
        ZmodemPhase::Running => tr(i18n_key::COMMON_TRANSFERRING_FILES),
        ZmodemPhase::Cancelling => tr(i18n_key::TERMINAL_CANCELLING),
        ZmodemPhase::Completed => tr(i18n_key::TERMINAL_COMPLETED),
        ZmodemPhase::Cancelled => tr(i18n_key::TERMINAL_CANCELLED),
        ZmodemPhase::Failed => tr(i18n_key::TERMINAL_FILE_TRANSFER_FAILED),
    }
}
