use super::*;
use rustterm::i18n::messages as i18n_key;
use rustterm::session::SftpTextDocument;
use std::sync::Arc;

pub(super) struct TextPreview {
    document: SftpTextDocument,
    pub(super) input: Entity<TextInput>,
    editable: bool,
    crlf: bool,
    external: Option<(Arc<tempfile::TempDir>, PathBuf)>,
    _subscription: Subscription,
}

impl FileTransferView {
    pub(super) fn render_conflict_policy(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .flex_wrap()
            .gap_2()
            .px_2()
            .py_1()
            .text_size(px(11.))
            .text_color(rgb(ui::MUTED))
            .child(tr(i18n_key::SFTP_EXISTING_FILES));
        for (id, label, policy) in [
            (
                "conflict-rename",
                i18n_key::TERMINAL_RENAME,
                SftpConflictPolicy::Rename,
            ),
            (
                "conflict-skip",
                i18n_key::SFTP_SKIP,
                SftpConflictPolicy::Skip,
            ),
            (
                "conflict-overwrite",
                i18n_key::SFTP_OVERWRITE,
                SftpConflictPolicy::Overwrite,
            ),
        ] {
            row = row.child(
                text_button(id, label, ButtonStyle::Neutral)
                    .tab_index(0)
                    .when(self.conflict_policy == policy, |d| {
                        d.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.prompts.is_blocked() {
                            this.conflict_policy = policy;
                            cx.notify();
                        }
                    })),
            );
        }
        row.into_any_element()
    }

    pub(super) fn retry_transfer(&mut self, id: usize, resume: bool, cx: &mut Context<Self>) {
        if self.closed || self.prompts.is_blocked() {
            return;
        }
        let Some(row) = self.transfers.iter_mut().find(|row| {
            row.id == id
                && matches!(
                    row.progress.state,
                    SftpTransferState::Failed(_) | SftpTransferState::Cancelled
                )
        }) else {
            return;
        };
        row.retry = if resume {
            row.transfer.take()
        } else {
            row.transfer = None;
            None
        };
        row.progress = SftpTransferProgress {
            bytes_done: 0,
            total_bytes: None,
            state: SftpTransferState::Queued,
        };
        row.cancel_requested = false;
        self.advance_queue();
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if !this
                    .update(cx, |this, cx| this.poll_transfers(cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        }));
        cx.emit(FileTransferEvent::Updated);
        cx.notify();
    }

    pub(super) fn preview_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed
            || self.loading
            || !self.directory_ready
            || self.mutating
            || self.native_pending
            || self.prompts.is_blocked()
        {
            return;
        }
        let entries = self.selected_entries();
        if entries.len() != 1 || entries[0].is_dir || entries[0].is_symlink {
            return;
        }
        if self.preview.is_some() {
            return;
        }
        let path = entries[0].path.clone();
        let client = self.client.clone();
        self.mutating = true;
        self.error = None;
        self.notice = None;
        let generation = self.generation;
        let task = cx
            .background_executor()
            .spawn(async move { client.read_text(&path).await });
        self.preview_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.mutating = false;
                if this.closed || this.generation != generation {
                    return;
                }
                match result {
                    Ok(document) => {
                        let crlf = document.text.contains("\r\n")
                            && !document.text.replace("\r\n", "").contains(['\r', '\n']);
                        let normalized = document.text.replace("\r\n", "\n");
                        let editable = normalized.len() <= 65_536
                            && !normalized
                                .chars()
                                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
                            && (!document.text.contains('\r') || crlf);
                        let visible: String = normalized
                            .chars()
                            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                            .scan(0usize, |bytes, ch| {
                                *bytes += ch.len_utf8();
                                (*bytes <= 65_536).then_some(ch)
                            })
                            .collect();
                        let input = cx.new(|cx| {
                            let mut input = TextInput::new(i18n_key::SFTP_REMOTE_TEXT, false, cx);
                            input.set_literal(true, cx);
                            input.set_value(&visible, cx);
                            input.set_read_only(!editable, cx);
                            input
                        });
                        let subscription =
                            cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                                match event {
                                    InputEvent::Submit => this.save_preview(false, window, cx),
                                    InputEvent::Rejected(message) => {
                                        this.error = Some((*message).into())
                                    }
                                    InputEvent::Changed => {}
                                }
                                cx.notify();
                            });
                        if !this.prompts.is_blocked() {
                            input.read(cx).focus(window);
                        }
                        this.preview = Some(TextPreview {
                            document,
                            input,
                            editable,
                            crlf,
                            external: None,
                            _subscription: subscription,
                        });
                    }
                    Err(error) => {
                        this.error = Some(rustterm::i18n::Message::with_arg(
                            i18n_key::SFTP_COULD_NOT_PREVIEW_FILE_ERROR,
                            "error",
                            rustterm::i18n::Message::from_error(error),
                        ))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn preview_dirty(&self, cx: &App) -> bool {
        self.preview.as_ref().is_some_and(|preview| {
            preview.external.is_some()
                || (preview.editable && preview_content(preview, cx) != preview.document.text)
        })
    }

    pub(super) fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mutating || self.prompts.is_blocked() {
            return;
        }
        if !self.preview_dirty(cx) {
            self.preview = None;
            self.focus.focus(window);
            cx.notify();
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let response = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::SFTP_CLOSE_THE_REMOTE_FILE_EDITOR),
            Some(tr(
                i18n_key::SFTP_UNUPLOADED_EDITS_WILL_BE_DISCARDED_CLOSE_ANY_EXTERNAL_EDITOR_FIRST,
            )),
            &[
                tr(i18n_key::COMMON_CANCEL),
                tr(i18n_key::SFTP_DISCARD_EDITS),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = response.await == Ok(1);
            let _ = this.update_in(cx, |this, window, cx| {
                let confirmed = confirmed && permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if confirmed {
                    this.preview = None;
                    this.focus.focus(window);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_external_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mutating || self.prompts.is_blocked() {
            return;
        }
        let Some(preview) = &self.preview else { return };
        if preview.external.is_some() {
            return;
        }
        let content = if preview.editable {
            preview_content(preview, cx)
        } else {
            preview.document.text.clone()
        };
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.mutating = true;
        let task = cx.background_executor().spawn(async move {
            let mut builder = tempfile::Builder::new();
            builder.prefix("rustterm-remote-edit-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let directory = Arc::new(builder.tempdir()?);
            let path = directory.path().join("remote-file.txt");
            std::fs::write(&path, content)?;
            if !permit.is_valid() {
                return Ok(None);
            }
            let mut child = std::process::Command::new(if cfg!(windows) {
                "notepad.exe"
            } else {
                "xdg-open"
            })
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
            drop(permit);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !cfg!(windows) {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(std::io::Error::other(tr(
                            i18n_key::SFTP_COULD_NOT_OPEN_THE_DESKTOP_TEXT_EDITOR,
                        )));
                    }
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::other(tr(
                        i18n_key::SFTP_THE_DESKTOP_EDITOR_DID_NOT_RESPOND_TRY_AGAIN,
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok::<_, std::io::Error>(Some((directory, path)))
        });
        self.preview_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.mutating = false;
                match result {
                    Ok(Some(external)) => {
                        if let Some(preview) = &mut this.preview {
                            preview.external = Some(external);
                            preview
                                .input
                                .update(cx, |input, cx| input.set_read_only(true, cx));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.error = Some(rustterm::i18n::Message::with_arg(
                            i18n_key::SFTP_COULD_NOT_OPEN_EDITOR_ERROR,
                            "error",
                            rustterm::i18n::Message::from_error(error),
                        ))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn save_preview(
        &mut self,
        external: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mutating || self.closed || self.prompts.is_blocked() {
            return;
        }
        let Some(preview) = &self.preview else { return };
        if preview.input.read(cx).is_composing()
            || (!external && (!preview.editable || preview.external.is_some()))
        {
            return;
        }
        let original = preview.document.clone();
        let external_file = if external {
            preview.external.clone()
        } else {
            None
        };
        if external && external_file.is_none() {
            return;
        }
        let content = preview_content(preview, cx);
        let client = self.client.clone();
        self.mutating = true;
        self.error = None;
        let task = cx.background_executor().spawn(async move {
            let content = if let Some((_directory, path)) = external_file {
                use std::io::Read;
                let mut data = Vec::new();
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                }
                let file = options.open(path)?;
                if !file.metadata()?.is_file() {
                    return Err(SftpError::Remote(
                        tr(i18n_key::SFTP_EXTERNAL_EDITS_MUST_BE_A_REGULAR_TEXT_FILE).into(),
                    ));
                }
                file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
                if data.len() > 1024 * 1024 {
                    return Err(SftpError::Remote(
                        tr(i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB).into(),
                    ));
                }
                String::from_utf8(data).map_err(|_| {
                    SftpError::Remote(
                        tr(i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB).into(),
                    )
                })?
            } else {
                content
            };
            client.save_text(original, content).await
        });
        self.preview_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.mutating = false;
                match result {
                    Ok(document) => {
                        if let Some(preview) = &mut this.preview {
                            preview.document = document;
                        }
                        this.notice = Some(i18n_key::SFTP_REMOTE_FILE_SAVED.into());
                        this.refresh_after_upload = true;
                    }
                    Err(error) => {
                        this.error = Some(rustterm::i18n::Message::with_arg(
                            i18n_key::SFTP_COULD_NOT_SAVE_REMOTE_FILE_ERROR,
                            "error",
                            rustterm::i18n::Message::from_error(error),
                        ))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = &self.preview else {
            return div().into_any_element();
        };
        let mut body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(
                div()
                    .text_color(rgb(ui::MUTED))
                    .child(preview.document.path.clone()),
            )
            .child(div().flex_1().min_h_0().child(preview.input.clone()));
        if !preview.editable {
            body = body.child(div().text_color(rgb(ui::MUTED)).child(tr(
                i18n_key::SFTP_LARGE_OR_SPECIAL_TEXT_IS_PREVIEWED_ONLY_USE_THE_EXTERNAL,
            )));
        }
        if preview.external.is_some() {
            body = body.child(div().text_color(rgb(ui::MUTED)).child(tr(
                i18n_key::SFTP_SAVE_IN_THE_EXTERNAL_EDITOR_THEN_UPLOAD_YOUR_EDITS_HERE,
            )));
        }
        if let Some(error) = &self.error {
            body = body.child(div().text_color(rgb(ui::DANGER)).child(error.render()));
        } else if let Some(notice) = &self.notice {
            body = body.child(div().text_color(rgb(ui::SUCCESS)).child(notice.render()));
        }
        let external = preview.external.is_some();
        body.child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    text_button(
                        "close-preview",
                        i18n_key::COMMON_CLOSE,
                        ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.close_preview(window, cx))),
                )
                .when(!external, |row| {
                    row.child(
                        text_button(
                            "external-editor",
                            i18n_key::SFTP_EXTERNAL_EDITOR,
                            ButtonStyle::Neutral,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.open_external_editor(window, cx)
                            }),
                        ),
                    )
                })
                .when(external || preview.editable, |row| {
                    row.child(
                        text_button(
                            "save-preview",
                            if self.mutating {
                                i18n_key::WORKSPACE_SAVING
                            } else {
                                i18n_key::SFTP_UPLOAD_EDITS
                            },
                            ButtonStyle::Primary,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.save_preview(external, window, cx),
                        )),
                    )
                }),
        )
        .into_any_element()
    }
}

fn preview_content(preview: &TextPreview, cx: &App) -> String {
    let content = preview.input.read(cx).value();
    if preview.crlf {
        content.replace('\n', "\r\n")
    } else {
        content.to_owned()
    }
}
