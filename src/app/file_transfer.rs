//! SFTP browser for an already authenticated SSH session. All remote operations run
//! outside GPUI; only active transfers have a progress timer, and listings are virtualized.

use rustterm::i18n::messages as i18n_key;
mod editing;
mod navigation;

use crate::app::{
    interaction::PromptGate,
    text_input::{InputEvent, TextInput},
    ui::{self, ButtonStyle, Icon, icon, icon_button, text_button},
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, PathPromptOptions, PromptLevel, Render,
    ScrollStrategy, Subscription, Task, Timer, UniformListScrollHandle, Window, div, prelude::*,
    px, rgb, uniform_list,
};
use rustterm::i18n::tr;
use rustterm::session::{
    SftpClient, SftpConflictPolicy, SftpEntry, SftpError, SftpTransfer, SftpTransferProgress,
    SftpTransferState,
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_TRANSFER_ROWS: usize = 128;
const ACTIVE_TRANSFERS: usize = 2;
const FILE_ROW_HEIGHT: f32 = 28.;

fn file_list_row(
    marker: impl IntoElement,
    name: impl IntoElement,
    kind: impl IntoElement,
    size: impl IntoElement,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(marker),
        )
        .child(div().flex_1().min_w_0().truncate().child(name))
        .child(div().w(px(50.)).flex_shrink_0().truncate().child(kind))
        .child(
            div()
                .w(px(74.))
                .flex_shrink_0()
                .text_right()
                .truncate()
                .child(size),
        )
}

#[derive(Clone, Copy, Debug)]
pub enum FileTransferEvent {
    Hide,
    Close,
    Updated,
    FollowDirectory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Upload,
    Download,
}

struct TransferRow {
    id: usize,
    name: String,
    local: PathBuf,
    remote: String,
    direction: Direction,
    transfer: Option<SftpTransfer>,
    progress: SftpTransferProgress,
    cancel_requested: bool,
    policy: SftpConflictPolicy,
    retry: Option<SftpTransfer>,
}

enum NameAction {
    CreateDirectory { directory: String },
    Rename { entry: SftpEntry, directory: String },
}

struct NameEditor {
    action: NameAction,
    input: Entity<TextInput>,
    _subscription: Subscription,
}

enum Mutation {
    CreateDirectory(String),
    Rename { old: String, new: String },
    Delete(Vec<SftpEntry>),
}

pub struct FileTransferView {
    client: SftpClient,
    prompts: PromptGate,
    focus: FocusHandle,
    path_input: Entity<TextInput>,
    _path_subscription: Subscription,
    _connection_task: Task<()>,
    directory: String,
    pending_directory: Option<String>,
    path_dirty: bool,
    entries: Vec<SftpEntry>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    generation: usize,
    loading: bool,
    directory_ready: bool,
    mutating: bool,
    native_pending: bool,
    closed: bool,
    follow_directory: bool,
    actions_open: bool,
    editor: Option<NameEditor>,
    error: Option<rustterm::i18n::Message>,
    notice: Option<rustterm::i18n::Message>,
    listing_task: Option<Task<()>>,
    mutation_task: Option<Task<()>>,
    progress_task: Option<Task<()>>,
    transfers: Vec<TransferRow>,
    next_transfer: usize,
    refresh_after_upload: bool,
    scroll: UniformListScrollHandle,
    transfer_scroll: UniformListScrollHandle,
    conflict_policy: SftpConflictPolicy,
    preview: Option<editing::TextPreview>,
    preview_task: Option<Task<()>>,
}

impl EventEmitter<FileTransferEvent> for FileTransferView {}

impl FileTransferView {
    pub fn new(
        client: SftpClient,
        prompts: PromptGate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_input = cx.new(|cx| {
            let mut input = TextInput::new(i18n_key::TERMINAL_REMOTE_PATH, false, cx);
            input.set_compact(true, cx);
            input.set_value(".", cx);
            input
        });
        let path_subscription = cx.subscribe_in(
            &path_input,
            window,
            |this, input, event, _, cx| match event {
                InputEvent::Submit => {
                    let path = input.read(cx).value().to_owned();
                    this.navigate(path, cx);
                }
                InputEvent::Changed => {
                    this.path_dirty = input.read(cx).value() != this.directory;
                    cx.notify();
                }
                InputEvent::Rejected(message) => {
                    this.error = Some((*message).into());
                    cx.notify();
                }
            },
        );
        let connection = client.clone();
        let connection_task = cx.spawn(async move |this, cx| {
            connection.closed().await;
            let _ = this.update(cx, |this, cx| {
                if !this.closed {
                    this.directory_ready = false;
                    this.error =
                        Some(i18n_key::CONNECTIONS_THE_SSH_CONNECTION_IS_DISCONNECTED.into());
                    cx.emit(FileTransferEvent::Updated);
                    cx.notify();
                }
            });
        });
        let mut view = Self {
            client,
            prompts,
            focus: cx.focus_handle(),
            path_input,
            _path_subscription: path_subscription,
            _connection_task: connection_task,
            directory: ".".into(),
            pending_directory: None,
            path_dirty: false,
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            generation: 0,
            loading: false,
            directory_ready: false,
            mutating: false,
            native_pending: false,
            closed: false,
            follow_directory: false,
            actions_open: false,
            editor: None,
            error: None,
            notice: None,
            listing_task: None,
            mutation_task: None,
            progress_task: None,
            transfers: Vec::new(),
            next_transfer: 0,
            refresh_after_upload: false,
            scroll: UniformListScrollHandle::default(),
            transfer_scroll: UniformListScrollHandle::default(),
            conflict_policy: SftpConflictPolicy::Rename,
            preview: None,
            preview_task: None,
        };
        view.navigate(".".into(), cx);
        view.focus.focus(window);
        view
    }

    pub fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    pub fn navigate_to(&mut self, path: String, cx: &mut Context<Self>) {
        if self.closed || self.prompts.is_blocked() {
            return;
        }
        if path.is_empty() || path.len() > 16 * 1024 || path.contains('\0') {
            self.error = Some(i18n_key::TERMINAL_ENTER_A_VALID_REMOTE_PATH.into());
            cx.notify();
            return;
        }
        self.pending_directory = Some(path);
        self.navigate_pending(cx);
    }

    pub fn cancel_pending_navigation(&mut self) {
        self.pending_directory = None;
    }

    fn navigate_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending_directory.is_none()
            || self.closed
            || self.loading
            || self.mutating
            || self.native_pending
            || self.prompts.is_blocked()
            || self.prompts.is_busy()
            || self.preview.is_some()
            || self.editor.is_some()
            || self.path_dirty
            || self.input_composing(cx)
        {
            return;
        }
        let path = self.pending_directory.take().expect("pending path checked");
        if !self.directory_ready || path != self.directory {
            self.navigate(path, cx);
        }
    }

    /// Includes the remote path and inline filename editor, not just the file list.
    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    pub fn input_composing(&self, cx: &App) -> bool {
        self.path_input.read(cx).is_composing()
            || self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.input.read(cx).is_composing())
            || self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.input.read(cx).is_composing())
    }

    pub fn refresh_language(&mut self, cx: &mut Context<Self>) {
        self.path_input.update(cx, |_, cx| cx.notify());
        if let Some(editor) = &self.editor {
            editor.input.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    pub fn has_active_transfers(&self) -> bool {
        self.active_transfer_count() > 0
    }

    pub fn transfer_count(&self) -> usize {
        self.transfers.len()
    }

    pub fn active_transfer_count(&self) -> usize {
        self.transfers
            .iter()
            .filter(|row| !finished(&row.progress.state))
            .count()
    }

    pub fn has_failed_transfers(&self) -> bool {
        self.transfers
            .iter()
            .any(|row| matches!(row.progress.state, SftpTransferState::Failed(_)))
    }

    pub fn has_pending_edits(&self, cx: &App) -> bool {
        self.editor.is_some() || self.preview_dirty(cx)
    }

    pub fn set_follow_directory(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.follow_directory != enabled {
            self.follow_directory = enabled;
            cx.notify();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.mutating || self.has_active_transfers()
    }

    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.native_pending || self.prompts.is_busy() || self.prompts.is_blocked()
        {
            return;
        }
        if self.mutating || self.input_composing(cx) {
            self.error = Some(
                i18n_key::TERMINAL_A_FILE_OPERATION_IS_FINISHING_PLEASE_WAIT_BEFORE_CLOSING.into(),
            );
            cx.notify();
            return;
        }
        if !self.has_active_transfers() && !self.has_pending_edits(cx) {
            self.finish_close(cx);
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let answer = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::SFTP_CANCEL_TRANSFERS_DISCARD_UNUPLOADED_EDITS_AND_CLOSE),
            Some(tr(i18n_key::SFTP_CLOSE_TRANSFERS_AND_EDITOR_WARNING)),
            &[
                tr(i18n_key::TERMINAL_CONTINUE_TRANSFERS),
                tr(i18n_key::TERMINAL_CANCEL_TRANSFERS_AND_CLOSE),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = this.update(cx, |this, cx| {
                let confirmed = confirmed && permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if confirmed {
                    this.finish_close(cx);
                }
                this.refresh_completed_uploads(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_close(&mut self, cx: &mut Context<Self>) {
        self.cancel_all();
        self.closed = true;
        self.listing_task = None;
        self.progress_task = None;
        cx.emit(FileTransferEvent::Close);
    }

    fn selected_entries(&self) -> Vec<SftpEntry> {
        self.selected
            .iter()
            .filter_map(|index| self.entries.get(*index).cloned())
            .collect()
    }

    fn row_mouse_down(
        &mut self,
        index: usize,
        generation: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading || self.mutating || generation != self.generation {
            return;
        }
        self.focus.focus(window);
        self.cursor = Some(index);
        if event.modifiers.shift {
            let anchor = self.anchor.unwrap_or(index);
            if !event.modifiers.control && !event.modifiers.platform {
                self.selected.clear();
            }
            self.selected.extend(anchor.min(index)..=anchor.max(index));
        } else if event.modifiers.control || event.modifiers.platform {
            if !self.selected.remove(&index) {
                self.selected.insert(index);
            }
            self.anchor = Some(index);
        } else {
            self.selected.clear();
            self.selected.insert(index);
            self.anchor = Some(index);
        }
        if event.click_count == 2
            && !event.modifiers.shift
            && !event.modifiers.control
            && !event.modifiers.platform
        {
            self.open_entry(index, window, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn open_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.loading || self.mutating || self.native_pending {
            return;
        }
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if entry.is_symlink {
            self.generation = self.generation.wrapping_add(1);
            let generation = self.generation;
            self.loading = true;
            self.error = None;
            self.notice = None;
            self.editor = None;
            let client = self.client.clone();
            let task = cx.background_executor().spawn(async move {
                client
                    .is_dir(&entry.path)
                    .await
                    .map(|is_dir| (entry, is_dir))
            });
            self.listing_task = Some(cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    if this.closed || generation != this.generation {
                        return;
                    }
                    this.loading = false;
                    match result {
                        Ok((entry, true)) => this.navigate(entry.path, cx),
                        Ok((entry, false)) => this.download_entries(vec![entry], cx),
                        Err(error) => {
                            this.error = Some(rustterm::i18n::Message::with_arg(
                                i18n_key::TERMINAL_COULD_NOT_OPEN_ITEM_ERROR,
                                "error",
                                rustterm::i18n::Message::from_error(error),
                            ));
                        }
                    }
                    this.refresh_completed_uploads(cx);
                    cx.notify();
                });
            }));
            cx.notify();
        } else if entry.is_dir {
            self.navigate(entry.path, cx);
        } else {
            self.download_entries(vec![entry], cx);
        }
        self.focus.focus(window);
    }

    fn begin_name(&mut self, rename: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.client.is_connected()
            || !self.directory_ready
            || self.loading
            || self.mutating
            || self.native_pending
        {
            return;
        }
        let (action, value) = if rename {
            let entries = self.selected_entries();
            if entries.len() != 1 {
                return;
            }
            let entry = entries.into_iter().next().expect("one selected entry");
            let name = entry.name.clone();
            (
                NameAction::Rename {
                    entry,
                    directory: self.directory.clone(),
                },
                name,
            )
        } else {
            (
                NameAction::CreateDirectory {
                    directory: self.directory.clone(),
                },
                String::new(),
            )
        };
        let input = cx.new(|cx| {
            let mut input = TextInput::new(i18n_key::TERMINAL_ENTER_A_NAME, false, cx);
            input.set_compact(true, cx);
            input.set_value(value, cx);
            input
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::Submit) {
                this.submit_name(window, cx);
            }
        });
        input.read(cx).focus(window);
        self.editor = Some(NameEditor {
            action,
            input,
            _subscription: subscription,
        });
        self.error = None;
        cx.notify();
    }

    fn submit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed
            || !self.directory_ready
            || self.loading
            || self.mutating
            || self.native_pending
            || self.prompts.is_busy()
            || self.prompts.is_blocked()
        {
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.input.read(cx).is_composing() {
            return;
        }
        let name = editor.input.read(cx).value().to_owned();
        if !valid_remote_name(&name) {
            self.error =
                Some(i18n_key::TERMINAL_NAMES_MUST_NOT_BE_EMPTY_OR_OR_CONTAIN_OR_NUL.into());
            cx.notify();
            return;
        }
        let editor = self.editor.take().expect("name editor exists");
        let operation = match editor.action {
            NameAction::CreateDirectory { directory } => {
                Mutation::CreateDirectory(remote_join(&directory, &name))
            }
            NameAction::Rename { entry, directory } => Mutation::Rename {
                old: entry.path,
                new: remote_join(&directory, &name),
            },
        };
        self.focus.focus(window);
        self.mutate(operation, cx);
    }

    fn cancel_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        self.focus.focus(window);
        self.refresh_completed_uploads(cx);
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed
            || !self.client.is_connected()
            || !self.directory_ready
            || self.loading
            || self.mutating
            || self.native_pending
            || self.editor.is_some()
        {
            return;
        }
        let entries = self.selected_entries();
        if entries.is_empty() {
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        let names = entries
            .iter()
            .take(5)
            .map(|entry| display_name(&entry.name))
            .collect::<Vec<_>>()
            .join("\n");
        let more = if entries.len() > 5 {
            rustterm::trf!(
                i18n_key::SFTP_ADDITIONAL_ITEM_COUNT,
                count = entries.len() - 5
            )
        } else {
            String::new()
        };
        let detail = rustterm::trf!(
            i18n_key::SFTP_NAMES_MORE_THIS_CANNOT_BE_UNDONE_ONLY_FILES_AND_EMPTY,
            names = names,
            more = more
        );
        self.native_pending = true;
        let answer = window.prompt(
            PromptLevel::Warning,
            &rustterm::trf!(
                i18n_key::SFTP_DELETE_SELECTED_CONFIRMATION,
                count = entries.len()
            ),
            Some(&detail),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_DELETE)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = this.update(cx, |this, cx| {
                let confirmed = confirmed && permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if confirmed && !this.closed {
                    this.mutate(Mutation::Delete(entries), cx);
                }
                this.refresh_completed_uploads(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn mutate(&mut self, operation: Mutation, cx: &mut Context<Self>) {
        if self.mutating || self.closed {
            return;
        }
        self.mutating = true;
        self.path_input
            .update(cx, |input, cx| input.set_read_only(true, cx));
        self.error = None;
        self.notice = None;
        let client = self.client.clone();
        let task = cx.background_executor().spawn(async move {
            match operation {
                Mutation::CreateDirectory(path) => client.mkdir(&path).await.map_err(|error| {
                    rustterm::i18n::Message::with_arg(
                        i18n_key::SFTP_COULD_NOT_CREATE_FOLDER_ERROR,
                        "error",
                        rustterm::i18n::Message::from_error(error),
                    )
                }),
                Mutation::Rename { old, new } => client.rename(&old, &new).await.map_err(|error| {
                    rustterm::i18n::Message::with_arg(
                        i18n_key::SFTP_COULD_NOT_RENAME_ITEM_ERROR,
                        "error",
                        rustterm::i18n::Message::from_error(error),
                    )
                }),
                Mutation::Delete(entries) => {
                    let mut failed = Vec::new();
                    for entry in entries {
                        if let Err(error) = client
                            .remove(&entry.path, entry.is_dir && !entry.is_symlink)
                            .await
                        {
                            failed.push(format!("{}: {error}", display_name(&entry.name)));
                        }
                    }
                    if failed.is_empty() {
                        Ok(())
                    } else {
                        let mut errors = failed
                            .iter()
                            .take(5)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        if failed.len() > 5 {
                            errors.push_str(&rustterm::trf!(
                                i18n_key::SFTP_ADDITIONAL_ITEM_COUNT,
                                count = failed.len() - 5,
                            ));
                        }
                        Err(rustterm::i18n::Message::with_arg(
                            i18n_key::SFTP_SOME_ITEMS_COULD_NOT_BE_DELETED_ERRORS,
                            "errors",
                            errors,
                        ))
                    }
                }
            }
        });
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.mutating = false;
                this.path_input
                    .update(cx, |input, cx| input.set_read_only(false, cx));
                if this.closed {
                    return;
                }
                this.refresh_directory(cx);
                match result {
                    Ok(()) => {
                        this.notice = Some(i18n_key::TERMINAL_FILE_OPERATION_COMPLETED.into())
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn choose_upload(&mut self, cx: &mut Context<Self>) {
        if !self.client.is_connected()
            || !self.directory_ready
            || self.loading
            || self.mutating
            || self.native_pending
        {
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let directory = self.directory.clone();
        let response = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some(tr(i18n_key::SFTP_SELECT_FILES_OR_FOLDERS_TO_UPLOAD).into()),
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
                        if paths.len() > MAX_TRANSFER_ROWS {
                            this.error = Some(rustterm::i18n::Message::with_arg(
                                i18n_key::SFTP_SELECT_AT_MOST_LIMIT_FILES_AT_A_TIME_UPLOAD_LARGER,
                                "limit",
                                MAX_TRANSFER_ROWS,
                            ));
                            this.refresh_completed_uploads(cx);
                            cx.notify();
                            return;
                        }
                        let mut jobs = Vec::new();
                        for local in paths {
                            let Some(name) = local
                                .file_name()
                                .and_then(|name| name.to_str())
                                .filter(|name| valid_remote_name(name))
                                .map(str::to_owned)
                            else {
                                this.error =
                                    Some(i18n_key::TERMINAL_A_SELECTED_FILE_HAS_AN_UNSUPPORTED_NAME_RENAME_IT_AND.into());
                                this.refresh_completed_uploads(cx);
                                cx.notify();
                                return;
                            };
                            jobs.push((
                                Direction::Upload,
                                local,
                                remote_join(&directory, &name),
                                name,
                            ));
                        }
                        this.enqueue(jobs, cx);
                    }
                    Ok(Ok(None)) => {}
                    _ => this.error = Some(i18n_key::COMMON_COULD_NOT_OPEN_THE_FILE_PICKER_PLEASE_TRY_AGAIN.into()),
                }
                this.refresh_completed_uploads(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn download_selected(&mut self, cx: &mut Context<Self>) {
        self.download_entries(self.selected_entries(), cx);
    }

    fn download_entries(&mut self, entries: Vec<SftpEntry>, cx: &mut Context<Self>) {
        if !self.client.is_connected()
            || !self.directory_ready
            || self.loading
            || self.mutating
            || self.native_pending
            || entries.is_empty()
        {
            return;
        }
        if entries
            .iter()
            .any(|entry| entry.is_symlink || !valid_component(&entry.name))
        {
            self.error = Some(
                i18n_key::SFTP_DIRECTORY_TRANSFERS_DO_NOT_FOLLOW_SYMBOLIC_LINKS_OR_SPECIAL_FILES
                    .into(),
            );
            cx.notify();
            return;
        }
        if entries.len() > MAX_TRANSFER_ROWS {
            self.error = Some(rustterm::i18n::Message::with_arg(
                i18n_key::SFTP_SELECT_AT_MOST_LIMIT_FILES_AT_A_TIME_DOWNLOAD_LARGER,
                "limit",
                MAX_TRANSFER_ROWS,
            ));
            cx.notify();
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        if entries.len() == 1 && !entries[0].is_dir {
            let entry = entries.into_iter().next().expect("one download entry");
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
            let response = cx.prompt_for_new_path(&home, Some(&entry.name));
            cx.spawn(async move |this, cx| {
                let response = response.await;
                let _ =
                    this.update(cx, |this, cx| {
                        let valid = permit.is_valid();
                        drop(permit);
                        this.native_pending = false;
                        if !valid {
                            cx.notify();
                            return;
                        }
                        match response {
                            Ok(Ok(Some(local))) => this.enqueue(
                                vec![(Direction::Download, local, entry.path, entry.name)],
                                cx,
                            ),
                            Ok(Ok(None)) => {}
                            _ => this.error = Some(
                                i18n_key::TERMINAL_COULD_NOT_OPEN_THE_SAVE_DIALOG_PLEASE_TRY_AGAIN
                                    .into(),
                            ),
                        }
                        this.refresh_completed_uploads(cx);
                        cx.notify();
                    });
            })
            .detach();
        } else {
            let response = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some(tr(i18n_key::TERMINAL_SELECT_A_DOWNLOAD_FOLDER).into()),
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
                            if let Some(directory) = paths.into_iter().next() {
                                this.enqueue(
                                    entries
                                        .into_iter()
                                        .map(|entry| {
                                            (
                                                Direction::Download,
                                                directory.join(&entry.name),
                                                entry.path,
                                                entry.name,
                                            )
                                        })
                                        .collect(),
                                    cx,
                                );
                            }
                        }
                        Ok(Ok(None)) => {}
                        _ => this.error = Some(
                            i18n_key::TERMINAL_COULD_NOT_OPEN_THE_FOLDER_PICKER_PLEASE_TRY_AGAIN
                                .into(),
                        ),
                    }
                    this.refresh_completed_uploads(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn enqueue(&mut self, jobs: Vec<(Direction, PathBuf, String, String)>, cx: &mut Context<Self>) {
        if self.closed || jobs.is_empty() {
            return;
        }
        let active = self
            .transfers
            .iter()
            .filter(|row| !finished(&row.progress.state))
            .count();
        if jobs.len() + active > MAX_TRANSFER_ROWS {
            self.error = Some(rustterm::i18n::Message::with_arg(
                i18n_key::SFTP_AT_MOST_LIMIT_FILES_CAN_BE_SCHEDULED_AT_ONCE_WAIT,
                "limit",
                MAX_TRANSFER_ROWS,
            ));
            return;
        }
        if self.transfers.len() + jobs.len() > MAX_TRANSFER_ROWS {
            self.transfers.retain(|row| !finished(&row.progress.state));
        }
        self.error = None;
        self.notice = None;
        for (direction, local, remote, name) in jobs {
            let id = self.next_transfer;
            self.next_transfer = self.next_transfer.wrapping_add(1);
            self.transfers.push(TransferRow {
                id,
                name,
                local,
                remote,
                direction,
                transfer: None,
                progress: SftpTransferProgress {
                    bytes_done: 0,
                    total_bytes: None,
                    state: SftpTransferState::Queued,
                },
                cancel_requested: false,
                policy: self.conflict_policy,
                retry: None,
            });
        }
        self.advance_queue();
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                let keep = this
                    .update(cx, |this, cx| this.poll_transfers(cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
        cx.emit(FileTransferEvent::Updated);
        cx.notify();
    }

    fn advance_queue(&mut self) -> bool {
        let mut slots = ACTIVE_TRANSFERS.saturating_sub(
            self.transfers
                .iter()
                .filter(|row| row.transfer.is_some() && !finished(&row.progress.state))
                .count(),
        );
        let mut changed = false;
        for row in &mut self.transfers {
            if slots == 0 {
                break;
            }
            if row.transfer.is_some() || !matches!(row.progress.state, SftpTransferState::Queued) {
                continue;
            }
            let result = if let Some(previous) = &row.retry {
                previous.retry(&self.client)
            } else {
                match row.direction {
                    Direction::Upload => {
                        self.client
                            .upload_tree(row.local.clone(), row.remote.clone(), row.policy)
                    }
                    Direction::Download => {
                        self.client
                            .download_tree(row.remote.clone(), row.local.clone(), row.policy)
                    }
                }
            };
            match result {
                Ok(transfer) => {
                    row.retry = None;
                    row.progress = transfer.progress();
                    if row.direction == Direction::Upload
                        && matches!(row.progress.state, SftpTransferState::Completed)
                    {
                        self.refresh_after_upload = true;
                    }
                    row.transfer = Some(transfer);
                    slots -= 1;
                }
                // Another channel may still be releasing its permit. Keep the item queued
                // instead of turning a short-lived limit into a failed user transfer.
                Err(SftpError::Busy) => break,
                Err(error) => row.progress.state = SftpTransferState::Failed(error.to_string()),
            }
            changed = true;
        }
        changed
    }

    fn poll_transfers(&mut self, cx: &mut Context<Self>) -> bool {
        if self.closed {
            return false;
        }
        let mut changed = false;
        for row in &mut self.transfers {
            if finished(&row.progress.state) {
                continue;
            }
            if let Some(transfer) = &row.transfer {
                let progress = transfer.progress();
                if progress != row.progress {
                    if matches!(progress.state, SftpTransferState::Completed)
                        && row.direction == Direction::Upload
                    {
                        self.refresh_after_upload = true;
                    }
                    row.progress = progress;
                    if matches!(row.progress.state, SftpTransferState::Completed) {
                        row.transfer = None;
                    }
                    changed = true;
                }
            }
        }
        changed |= self.advance_queue();
        let active = self.has_active_transfers();
        if !active {
            self.refresh_completed_uploads(cx);
        }
        if changed {
            cx.emit(FileTransferEvent::Updated);
            cx.notify();
        }
        active
    }

    fn refresh_completed_uploads(&mut self, cx: &mut Context<Self>) {
        if self.closed
            || !self.refresh_after_upload
            || self.has_active_transfers()
            || self.mutating
            || self.native_pending
            || self.loading
            || self.editor.is_some()
            || self.preview.is_some()
            || self.path_input.read(cx).value() != self.directory
        {
            return;
        }
        self.refresh_after_upload = false;
        let error = self.error.take();
        let notice = self.notice.take();
        self.refresh_directory(cx);
        self.error = error;
        self.notice = notice;
    }

    fn show_transfer_error(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.transfers.iter().find(|row| row.id == id) else {
            return;
        };
        let SftpTransferState::Failed(error) = &row.progress.state else {
            return;
        };
        let detail = format!("{}\n\n{}", display_name(&row.name), error);
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let answer = window.prompt(
            PromptLevel::Warning,
            tr(i18n_key::TERMINAL_FILE_TRANSFER_FAILED),
            Some(&detail),
            &[tr(i18n_key::COMMON_CLOSE)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let _ = answer.await;
            drop(permit);
            let _ = this.update(cx, |this, cx| {
                this.native_pending = false;
                this.refresh_completed_uploads(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_transfer(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            row.cancel_requested = true;
            if let Some(transfer) = &row.transfer {
                transfer.cancel();
            } else {
                row.progress.state = SftpTransferState::Cancelled;
            }
        }
        cx.emit(FileTransferEvent::Updated);
        cx.notify();
    }

    fn cancel_all(&mut self) {
        for row in &mut self.transfers {
            if !finished(&row.progress.state) {
                row.cancel_requested = true;
                if let Some(transfer) = &row.transfer {
                    transfer.cancel();
                } else {
                    row.progress.state = SftpTransferState::Cancelled;
                }
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.native_pending
            || self.prompts.is_busy()
            || self.prompts.is_blocked()
            || self.input_composing(cx)
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if self.preview.is_some() {
            if key == "escape" {
                self.close_preview(window, cx);
            } else if key == "s" && modifiers.control {
                self.save_preview(false, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if key == "escape" {
            if self.editor.is_some() {
                self.cancel_name(window, cx);
            } else if self.actions_open {
                self.actions_open = false;
                cx.notify();
            } else if self.path_input.read(cx).focus_handle().is_focused(window) {
                self.cancel_navigation(cx);
                self.focus.focus(window);
            } else {
                cx.emit(FileTransferEvent::Hide);
            }
        } else if (modifiers.control || modifiers.platform) && key == "l" {
            self.path_input.update(cx, |input, cx| input.select_all(cx));
            self.path_input.read(cx).focus(window);
        } else if self.focus.is_focused(window) && !self.loading && !self.mutating {
            match key {
                "f5" => self.refresh_directory(cx),
                "backspace" | "up" if key == "backspace" || modifiers.alt => {
                    self.navigate(remote_parent(&self.directory), cx)
                }
                "enter" => {
                    if let Some(index) = self
                        .cursor
                        .filter(|index| self.selected.contains(index))
                        .or_else(|| self.selected.first().copied())
                    {
                        self.open_entry(index, window, cx);
                    }
                }
                "f2" => self.begin_name(true, window, cx),
                "f4" => self.preview_selected(window, cx),
                "delete" => self.confirm_delete(window, cx),
                "a" if modifiers.control || modifiers.platform => {
                    self.selected.extend(0..self.entries.len());
                    self.anchor = Some(0);
                    self.cursor = self.entries.len().checked_sub(1);
                    cx.notify();
                }
                "up" | "down" | "home" | "end" | "pageup" | "page-up" | "pagedown"
                | "page-down"
                    if !self.entries.is_empty() =>
                {
                    let current = self.cursor.unwrap_or(0);
                    let page = (f32::from(self.scroll.0.borrow().base_handle.bounds().size.height)
                        / FILE_ROW_HEIGHT)
                        .floor()
                        .max(1.) as usize;
                    let last = self.entries.len() - 1;
                    let index = match key {
                        "up" => current.saturating_sub(1),
                        "down" => (current + usize::from(self.cursor.is_some())).min(last),
                        "home" => 0,
                        "end" => last,
                        "pageup" | "page-up" => current.saturating_sub(page),
                        _ => current.saturating_add(page).min(last),
                    };
                    if !modifiers.shift {
                        self.selected.clear();
                        self.anchor = Some(index);
                        self.selected.insert(index);
                    } else {
                        let anchor = self.anchor.unwrap_or(current);
                        self.anchor = Some(anchor);
                        if !modifiers.control && !modifiers.platform {
                            self.selected.clear();
                        }
                        self.selected.extend(anchor.min(index)..=anchor.max(index));
                    }
                    self.cursor = Some(index);
                    self.scroll.scroll_to_item(index, ScrollStrategy::Center);
                    cx.notify();
                }
                _ => return,
            }
        } else {
            return;
        }
        cx.stop_propagation();
        window.prevent_default();
    }

    fn unavailable_reason(&self, needs_directory: bool) -> Option<&'static str> {
        if self.closed || !self.client.is_connected() {
            Some(i18n_key::CONNECTIONS_THE_SSH_CONNECTION_IS_DISCONNECTED)
        } else if self.native_pending || self.prompts.is_busy() || self.prompts.is_blocked() {
            Some(i18n_key::COMMON_FINISH_THE_OPEN_DIALOG_FIRST)
        } else if self.mutating {
            Some(i18n_key::COMMON_WAIT_FOR_THE_CURRENT_FILE_OPERATION_TO_FINISH)
        } else if self.editor.is_some() || self.preview.is_some() {
            Some(i18n_key::COMMON_CLOSE_THE_FILE_EDITOR_BEFORE_BROWSING)
        } else if needs_directory && self.loading {
            Some(i18n_key::COMMON_LOADING_DIRECTORY)
        } else if needs_directory && !self.directory_ready {
            Some(i18n_key::COMMON_OPEN_A_REMOTE_DIRECTORY_FIRST)
        } else {
            None
        }
    }

    fn render_file_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let unavailable = self.unavailable_reason(true);
        let selected = self.selected.len();
        let one_item =
            unavailable.or((selected != 1).then_some(i18n_key::COMMON_SELECT_EXACTLY_ONE_ITEM));
        let any_item =
            unavailable.or((selected == 0).then_some(i18n_key::COMMON_SELECT_AT_LEAST_ONE_ITEM));
        let regular_file = selected == 1
            && self
                .selected
                .first()
                .and_then(|index| self.entries.get(*index))
                .is_some_and(|entry| !entry.is_dir && !entry.is_symlink);
        let preview_unavailable = unavailable
            .or((!regular_file).then_some(i18n_key::COMMON_SELECT_ONE_REGULAR_FILE_TO_PREVIEW));
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::SURFACE))
            .border_b_1()
            .border_color(rgb(ui::BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_1()
                    .p_2()
                    .text_size(px(11.))
                    .child(
                        ui::text_button_enabled(
                            "preview-remote",
                            i18n_key::WORKSPACE_PREVIEW_EDIT,
                            ButtonStyle::Neutral,
                            preview_unavailable.is_none(),
                        )
                        .when_some(preview_unavailable, |button, reason| {
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.actions_open = false;
                            this.preview_selected(window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::text_button_enabled(
                            "mkdir",
                            i18n_key::TERMINAL_NEW_FOLDER,
                            ButtonStyle::Neutral,
                            unavailable.is_none(),
                        )
                        .when_some(unavailable, |button, reason| {
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.actions_open = false;
                            this.begin_name(false, window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::text_button_enabled(
                            "rename",
                            i18n_key::TERMINAL_RENAME,
                            ButtonStyle::Neutral,
                            one_item.is_none(),
                        )
                        .when_some(one_item, |button, reason| {
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.actions_open = false;
                            this.begin_name(true, window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::text_button_enabled(
                            "delete",
                            i18n_key::COMMON_DELETE,
                            ButtonStyle::Danger,
                            any_item.is_none(),
                        )
                        .text_color(rgb(ui::DANGER))
                        .when_some(any_item, |button, reason| {
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.actions_open = false;
                            this.confirm_delete(window, cx);
                            cx.notify();
                        })),
                    ),
            )
            .child(self.render_conflict_policy(cx))
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(rgb(ui::BORDER))
                    .child(
                        text_button(
                            "disconnect-file-session",
                            i18n_key::WORKSPACE_DISCONNECT_FILE_SESSION,
                            ButtonStyle::Danger,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.actions_open = false;
                            this.request_close(window, cx);
                            cx.notify();
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let entry = &self.entries[index];
        let generation = self.generation;
        file_list_row(
            icon(if entry.is_dir {
                Icon::Folder
            } else if entry.is_symlink {
                Icon::Link
            } else {
                Icon::File
            }),
            display_name(&entry.name),
            div()
                .text_color(rgb(ui::MUTED))
                .text_size(px(11.))
                .child(if entry.is_symlink {
                    tr(i18n_key::TERMINAL_LINK)
                } else if entry.is_dir {
                    tr(i18n_key::TERMINAL_FOLDER)
                } else {
                    tr(i18n_key::COMMON_FILE)
                }),
            div()
                .text_color(rgb(ui::MUTED))
                .text_size(px(11.))
                .child(if entry.is_dir {
                    "—".into()
                } else {
                    format_bytes(entry.size)
                }),
        )
        .id(("remote-file", index))
        .h(px(FILE_ROW_HEIGHT))
        .when(self.loading, |d| d.opacity(0.5))
        .when(self.selected.contains(&index), |d| d.bg(rgb(ui::SELECTED)))
        .hover(|s| s.bg(rgb(ui::HOVER)))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event, window, cx| {
                this.row_mouse_down(index, generation, event, window, cx)
            }),
        )
        .into_any_element()
    }

    fn render_transfer(&self, row: &TransferRow, cx: &mut Context<Self>) -> AnyElement {
        let id = row.id;
        let (state, color) = if row.cancel_requested && !finished(&row.progress.state) {
            (tr(i18n_key::TERMINAL_CANCELLING).to_owned(), ui::MUTED)
        } else {
            match &row.progress.state {
                SftpTransferState::Queued => (tr(i18n_key::TERMINAL_QUEUED).to_owned(), ui::MUTED),
                SftpTransferState::Running => (
                    match row.progress.total_bytes {
                        Some(total) => format!(
                            "{} / {}",
                            format_bytes(row.progress.bytes_done),
                            format_bytes(total)
                        ),
                        None => format_bytes(row.progress.bytes_done),
                    },
                    ui::ACCENT,
                ),
                SftpTransferState::Completed => {
                    (tr(i18n_key::TERMINAL_COMPLETED).to_owned(), ui::SUCCESS)
                }
                SftpTransferState::Cancelled => {
                    (tr(i18n_key::TERMINAL_CANCELLED).to_owned(), ui::MUTED)
                }
                SftpTransferState::Failed(error) => (
                    rustterm::trf!(i18n_key::SFTP_FAILED_ERROR, error = error),
                    ui::DANGER,
                ),
            }
        };
        div()
            .id(("transfer", id))
            .w_full()
            .min_w_0()
            .h(px(28.))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .child(div().w(px(32.)).text_color(rgb(ui::MUTED)).child(
                if row.direction == Direction::Upload {
                    tr(i18n_key::TERMINAL_UPLOAD)
                } else {
                    tr(i18n_key::TERMINAL_DOWNLOAD)
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(display_name(&row.name)),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(color))
                    .child(state),
            )
            .when(
                !finished(&row.progress.state) && !row.cancel_requested,
                |d| {
                    d.child(
                        icon_button(
                            ("cancel-transfer", id),
                            Icon::Stop,
                            i18n_key::TERMINAL_CANCEL_TRANSFER,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(move |this, _, _, cx| this.cancel_transfer(id, cx))),
                    )
                },
            )
            .when(
                matches!(
                    row.progress.state,
                    SftpTransferState::Failed(_) | SftpTransferState::Cancelled
                ),
                |d| {
                    d.child(
                        icon_button(
                            ("retry-transfer", id),
                            Icon::Resume,
                            i18n_key::SFTP_RESUME_INTERRUPTED_TRANSFER,
                        )
                        .tab_index(0)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.retry_transfer(id, true, cx)),
                        ),
                    )
                    .child(
                        icon_button(
                            ("restart-transfer", id),
                            Icon::Refresh,
                            i18n_key::SFTP_RESTART_TRANSFER,
                        )
                        .tab_index(0)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.retry_transfer(id, false, cx)),
                        ),
                    )
                },
            )
            .when(
                matches!(row.progress.state, SftpTransferState::Failed(_)),
                |d| {
                    d.child(
                        icon_button(
                            ("transfer-error", id),
                            Icon::Warning,
                            i18n_key::TERMINAL_VIEW_ERROR,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.show_transfer_error(id, window, cx),
                        )),
                    )
                },
            )
            .into_any_element()
    }
}

impl Focusable for FileTransferView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for FileTransferView {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

impl Render for FileTransferView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.navigate_pending(cx);
        let unavailable = self.unavailable_reason(true);
        let navigation_unavailable = self.unavailable_reason(false);
        let selected = self.selected.len();
        let download_unavailable =
            unavailable.or((selected == 0).then_some(i18n_key::COMMON_SELECT_AT_LEAST_ONE_ITEM));
        let path_bar = div()
            .h(px(38.))
            .flex_shrink_0()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(rgb(ui::BORDER))
            .child(
                ui::icon_button_enabled(
                    "remote-parent",
                    Icon::ChevronUp,
                    navigation_unavailable.unwrap_or(i18n_key::TERMINAL_PARENT_FOLDER_BACKSPACE),
                    navigation_unavailable.is_none(),
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.navigate(remote_parent(&this.directory), cx)),
                ),
            )
            .child(
                ui::icon_button_enabled(
                    "remote-refresh",
                    Icon::Refresh,
                    navigation_unavailable.unwrap_or(i18n_key::TERMINAL_REFRESH_F5),
                    navigation_unavailable.is_none(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.refresh_directory(cx))),
            )
            .child(div().flex_1().min_w_0().child(self.path_input.clone()))
            .child(
                ui::text_button_enabled(
                    "remote-go",
                    if self.loading {
                        i18n_key::COMMON_LOADING
                    } else {
                        i18n_key::COMMON_GO
                    },
                    ButtonStyle::Neutral,
                    navigation_unavailable.is_none(),
                )
                .when_some(navigation_unavailable, |button, reason| {
                    button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    let path = this.path_input.read(cx).value().to_owned();
                    this.navigate(path, cx);
                })),
            );
        let toolbar = div()
            .h(px(38.))
            .flex_shrink_0()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(11.))
            .child(
                ui::text_button_enabled(
                    "upload",
                    i18n_key::TERMINAL_UPLOAD,
                    ButtonStyle::Primary,
                    unavailable.is_none(),
                )
                .flex_shrink_0()
                .when_some(unavailable, |button, reason| {
                    button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                })
                .on_click(cx.listener(|this, _, _, cx| this.choose_upload(cx))),
            )
            .child(
                ui::text_button_enabled(
                    "download",
                    i18n_key::TERMINAL_DOWNLOAD,
                    ButtonStyle::Neutral,
                    download_unavailable.is_none(),
                )
                .flex_shrink_0()
                .when_some(download_unavailable, |button, reason| {
                    button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                })
                .on_click(cx.listener(|this, _, _, cx| this.download_selected(cx))),
            )
            .child(div().flex_1())
            .child(
                text_button(
                    "follow-terminal",
                    i18n_key::WORKSPACE_FOLLOW_TERMINAL,
                    ButtonStyle::Neutral,
                )
                .tab_index(0)
                .flex_shrink_0()
                .when(self.follow_directory, |button| {
                    button.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                })
                .tooltip({
                    let label = if self.follow_directory {
                        i18n_key::WORKSPACE_STOP_FOLLOWING_TERMINAL_DIRECTORY
                    } else {
                        i18n_key::WORKSPACE_FOLLOW_TERMINAL_DIRECTORY_IN_SFTP
                    };
                    move |_, cx| cx.new(|_| ui::Tooltip(tr(label).into())).into()
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.closed && !this.native_pending && !this.prompts.is_blocked() {
                        cx.emit(FileTransferEvent::FollowDirectory);
                    }
                })),
            )
            .child(
                icon_button(
                    "file-actions",
                    Icon::More,
                    i18n_key::WORKSPACE_MORE_FILE_ACTIONS,
                )
                .tab_index(0)
                .when(self.actions_open, |button| button.bg(rgb(ui::SELECTED)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.actions_open = !this.actions_open;
                    cx.notify();
                })),
            );
        let header = file_list_row(
            div(),
            tr(i18n_key::COMMON_NAME),
            tr(i18n_key::COMMON_TYPE),
            tr(i18n_key::COMMON_SIZE),
        )
        .pr(px(10. + ui::SCROLLBAR_WIDTH))
        .h(px(24.))
        .flex_shrink_0()
        .text_size(px(10.))
        .text_color(rgb(ui::MUTED))
        .bg(rgb(ui::SURFACE))
        .border_y_1()
        .border_color(rgb(ui::BORDER));
        let mut panel = div()
            .id("sftp-files")
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(ui::SURFACE))
            .text_color(rgb(ui::TEXT))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // Inputs stop propagation themselves. Clicking empty chrome or
                    // a toolbar action makes this dock the keyboard destination.
                    this.focus.focus(window);
                    cx.stop_propagation();
                }),
            )
            .child(path_bar)
            .child(toolbar);
        if self.actions_open {
            panel = panel.child(self.render_file_actions(cx));
        }
        if self.preview.is_some() {
            return panel.child(self.render_preview(cx));
        }
        if let Some(editor) = &self.editor {
            panel = panel.child(
                div()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .bg(rgb(ui::SURFACE))
                    .child(
                        if matches!(editor.action, NameAction::CreateDirectory { .. }) {
                            tr(i18n_key::TERMINAL_FOLDER_NAME)
                        } else {
                            tr(i18n_key::TERMINAL_NEW_NAME)
                        },
                    )
                    .child(div().flex_1().min_w_0().child(editor.input.clone()))
                    .child(
                        text_button("commit-name", i18n_key::COMMON_OK, ButtonStyle::Primary)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_name(window, cx)),
                            ),
                    )
                    .child(
                        text_button("cancel-name", i18n_key::COMMON_CANCEL, ButtonStyle::Neutral)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cancel_name(window, cx)),
                            ),
                    ),
            );
        }
        panel = panel.child(header);
        if self.entries.is_empty() {
            panel = panel.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(ui::MUTED))
                    .child(if self.loading {
                        tr(i18n_key::TERMINAL_LOADING_FILES)
                    } else if !self.directory_ready {
                        tr(i18n_key::TERMINAL_COULD_NOT_READ_THIS_DIRECTORY_CHANGE_THE_PATH_AND_TRY)
                    } else {
                        tr(i18n_key::TERMINAL_THIS_DIRECTORY_IS_EMPTY)
                    }),
            );
        } else {
            panel = panel.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        uniform_list(
                            "remote-files",
                            self.entries.len(),
                            cx.processor(
                                |this, range: std::ops::Range<usize>, _, cx| -> Vec<AnyElement> {
                                    range.map(|index| this.render_row(index, cx)).collect()
                                },
                            ),
                        )
                        .map(ui::preserve_scroll_axes)
                        .track_scroll(self.scroll.clone())
                        .w_full()
                        .h_full()
                        .min_w_0()
                        .min_h_0(),
                    )
                    .child(ui::scrollbars(
                        "sftp-files-scrollbar",
                        None,
                        Some(self.scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        if !self.transfers.is_empty() {
            panel = panel.child(
                div()
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(ui::BORDER))
                    .bg(rgb(ui::SURFACE))
                    .text_size(px(11.))
                    .child(rustterm::trf!(
                        i18n_key::SFTP_TRANSFER_ITEM_COUNT,
                        count = self.transfers.len()
                    ))
                    .when(
                        self.transfers
                            .iter()
                            .any(|row| !finished(&row.progress.state) && !row.cancel_requested),
                        |header| {
                            header.child(
                                icon_button(
                                    "cancel-all-transfers",
                                    Icon::Stop,
                                    i18n_key::TERMINAL_CANCEL_ALL_UNFINISHED_TRANSFERS,
                                )
                                .tab_index(0)
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.cancel_all();
                                        cx.emit(FileTransferEvent::Updated);
                                        cx.notify();
                                    },
                                )),
                            )
                        },
                    ),
            );
            panel = panel.child(
                div()
                    .relative()
                    .min_w_0()
                    .h(px((self.transfers.len().min(4) * 28) as f32))
                    .flex_shrink_0()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        uniform_list(
                            "file-transfers",
                            self.transfers.len(),
                            cx.processor(
                                |this, range: std::ops::Range<usize>, _, cx| -> Vec<AnyElement> {
                                    range
                                        .map(|index| {
                                            this.render_transfer(&this.transfers[index], cx)
                                        })
                                        .collect()
                                },
                            ),
                        )
                        .map(ui::preserve_scroll_axes)
                        .w_full()
                        .min_w_0()
                        .h_full()
                        .track_scroll(self.transfer_scroll.clone()),
                    )
                    .child(ui::scrollbars(
                        "sftp-transfers-scrollbar",
                        None,
                        Some(self.transfer_scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        if let Some(error) = &self.error {
            panel = panel.child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .flex_shrink_0()
                    .text_color(rgb(ui::DANGER))
                    .child(error.render()),
            );
        } else if let Some(notice) = &self.notice {
            panel = panel.child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .flex_shrink_0()
                    .text_color(rgb(ui::SUCCESS))
                    .child(notice.render()),
            );
        }
        panel.child(
            div()
                .h(px(24.))
                .flex_shrink_0()
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(rgb(ui::BORDER))
                .text_size(px(10.))
                .text_color(rgb(ui::MUTED))
                .child(div().flex_1().min_w_0().truncate().child(if self.mutating {
                    tr(i18n_key::TERMINAL_PROCESSING_FILE_OPERATION).into()
                } else {
                    rustterm::trf!(
                        i18n_key::SFTP_SELECTED_ITEM_COUNT,
                        count = self.entries.len(),
                        selected = selected
                    )
                }))
                .child(icon_button(
                    "file-safety",
                    Icon::Shield,
                    i18n_key::SFTP_DOUBLE_CLICK_TO_OPEN_F4_TO_PREVIEW_CTRL_SHIFT_TO,
                )),
        )
    }
}

fn finished(state: &SftpTransferState) -> bool {
    matches!(
        state,
        SftpTransferState::Completed | SftpTransferState::Cancelled | SftpTransferState::Failed(_)
    )
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['\0', '/'])
}

fn valid_component(name: &str) -> bool {
    if name.contains('\0') {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(component)) if component == name)
        && components.next().is_none()
}

fn remote_join(directory: &str, name: &str) -> String {
    format!("{}/{name}", directory.trim_end_matches('/'))
}

fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((parent, _)) => parent.into(),
        None if path.starts_with('/') => "/".into(),
        None => ".".into(),
    }
}

fn display_name(name: &str) -> String {
    let mut display = String::with_capacity(name.len().min(1024));
    for (index, ch) in name.chars().enumerate() {
        if index == 512 {
            display.push('…');
            break;
        }
        if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            display.extend(ch.escape_default());
        } else {
            display.push(ch);
        }
    }
    display
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names_cannot_escape_the_selected_directory() {
        for name in [
            "",
            ".",
            "..",
            "/tmp/file",
            "../file",
            "a/b",
            "a/",
            "a/.",
            "bad\0name",
        ] {
            assert!(!valid_component(name), "unexpected filename: {name:?}");
            assert!(
                !valid_remote_name(name),
                "unexpected remote filename: {name:?}"
            );
        }
        for name in ["报告 2026.txt", ".profile", "name with spaces", "a..b"] {
            assert!(valid_component(name));
            assert!(valid_remote_name(name));
            assert_eq!(
                Path::new("/chosen").join(name).parent(),
                Some(Path::new("/chosen"))
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_download_names_preserve_colons_and_backslashes() {
        for name in ["backup:2026", "a\\b", "C:report", "..\\report"] {
            assert!(valid_remote_name(name));
            assert!(valid_component(name));
            let target = Path::new("/chosen").join(name);
            assert_eq!(target.parent(), Some(Path::new("/chosen")));
            assert_eq!(target.file_name().unwrap(), name);
        }
    }

    #[test]
    fn parent_navigation_stays_at_the_remote_root() {
        assert_eq!(remote_parent("/"), "/");
        assert_eq!(remote_parent("/home/user/"), "/home");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("."), ".");
        assert_eq!(remote_join("/", "file"), "/file");
        assert_eq!(remote_join("/home/user/", "file"), "/home/user/file");
    }

    #[test]
    fn filenames_show_control_characters_without_mutating_operation_paths() {
        let name = "report\n\u{202e}txt";
        assert_eq!(display_name(name), "report\\n\\u{202e}txt");
        assert!(valid_component(name));
        assert_eq!(remote_join("/data", name), format!("/data/{name}"));
        assert!(display_name(&"x".repeat(10_000)).chars().count() <= 513);
    }
}
