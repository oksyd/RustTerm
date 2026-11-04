//! Native Redis workspace. Network requests are explicit, bounded and asynchronous;
//! the view never polls a database while idle or hands Redis traffic to a PTY.

use rustterm::i18n::messages as i18n_key;
mod chrome;
mod connection;
mod editing;
mod operations;
mod render;
mod value;

use crate::app::{
    interaction::WindowState,
    layout::Axis,
    pane::PaneEvent,
    text_input::{InputEvent, TextInput},
    ui,
};
use gpui::{
    App, AppContext, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, Pixels, PromptLevel, ScrollStrategy, Subscription, Task, UniformListScrollHandle,
    Window,
};
use rustterm::{
    config::{RedisConfig, Secret, Settings},
    i18n::{Message, tr},
    redis::{KeyData, KeyInfo, KeyPage, KeyType, PageRequest, RedisClient, RedisError, RedisValue},
    session::SshOptions,
};
use std::{future::Future, time::Duration};

const MAX_CONSOLE_BYTES: usize = 128 * 1024;
const MAX_COMMAND_HISTORY: usize = 64;
pub const BROWSER_MIN_WIDTH: f32 = 680.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Connecting,
    WaitingHostKey,
    Connected,
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataKind {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

impl DataKind {
    const ALL: [Self; 6] = [
        Self::String,
        Self::Hash,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Stream,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Hash => "Hash",
            Self::List => "List",
            Self::Set => "Set",
            Self::ZSet => "ZSet",
            Self::Stream => "Stream",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedisMenu {
    Section,
    Pane,
    Key,
    Entry,
}

enum EditAction {
    Create(DataKind),
    Rename,
    Ttl,
    String,
    Hash(Option<Vec<u8>>),
    List(Option<(u64, Vec<u8>)>),
    Set,
    ZSet(Option<Vec<u8>>),
    Stream,
}

struct Editor {
    action: EditAction,
    encoding: value::EditEncoding,
    name: Entity<TextInput>,
    value: Entity<TextInput>,
    extra: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
enum Navigation {
    Keys,
    Values,
}

#[derive(Clone, Copy)]
enum ScanIntent {
    Refresh,
    Navigate,
    Database(u32),
}

enum Completion {
    Scan {
        page: KeyPage,
        cursor: u64,
        pattern: Vec<u8>,
        history: Vec<u64>,
        intent: ScanIntent,
    },
    Inspect {
        info: Result<KeyInfo, RedisError>,
        key: Vec<u8>,
        page: PageRequest,
        history: Vec<PageRequest>,
    },
    Mutation {
        key: Option<Vec<u8>>,
        accepted: bool,
        rejection: &'static str,
    },
    Reload {
        page: KeyPage,
        detail: Option<(Vec<u8>, KeyInfo)>,
        detail_page: PageRequest,
    },
    Console {
        result: Result<RedisValue, RedisError>,
        revision: usize,
    },
}

#[derive(Clone, Copy)]
enum ExpectedReply {
    Any,
    Nonzero,
    Present,
}

/// Resolved connection settings and short-lived credentials. The workspace owns
/// profile lookup and credential collection; this view owns the connection.
pub struct RedisLaunch {
    pub config: RedisConfig,
    pub secret: Option<Secret>,
    pub sentinel_secret: Option<Secret>,
    pub ssh: Option<SshOptions>,
}

enum ViewError {
    Message(&'static str),
    Backend(RedisError),
}

impl ViewError {
    fn render(&self) -> String {
        match self {
            Self::Message(message) => tr(message).to_owned(),
            Self::Backend(error) => {
                rustterm::trf!(i18n_key::REDIS_REDIS_ERROR, error = error.to_string())
            }
        }
    }
}

impl From<&'static str> for ViewError {
    fn from(message: &'static str) -> Self {
        Self::Message(message)
    }
}

pub struct RedisView {
    ops: operations::Operations,
    index: usize,
    title: String,
    pane_close_visible: bool,
    config: RedisConfig,
    ssh_route: Option<String>,
    client: Option<RedisClient>,
    state: ConnectionState,
    manual_disconnect: bool,
    retryable_failure: bool,
    window_state: WindowState,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    database_input: Entity<TextInput>,
    filter_input: Entity<TextInput>,
    command_input: Entity<TextInput>,
    command_revision: usize,
    command_history: Vec<String>,
    command_history_index: Option<usize>,
    command_history_draft: String,
    database: u32,
    keys: Vec<Vec<u8>>,
    scan_cursor: u64,
    scan_next: u64,
    scan_history: Vec<u64>,
    scan_pattern: Vec<u8>,
    scanned: bool,
    selected_key: Option<Vec<u8>>,
    info: Option<KeyInfo>,
    detail_page: PageRequest,
    detail_history: Vec<PageRequest>,
    selected_row: Option<usize>,
    navigation: Navigation,
    editor: Option<Editor>,
    console_open: bool,
    console_height: f32,
    resizing_console: bool,
    menu: Option<RedisMenu>,
    menu_position: Option<gpui::Point<Pixels>>,
    menu_index: usize,
    menu_scroll: gpui::ScrollHandle,
    menu_return_focus: Option<FocusHandle>,
    compact: bool,
    viewport_size: (f32, f32),
    browser_open: bool,
    browser_collapsed: bool,
    browser_width: f32,
    resizing_browser: bool,
    raw_preview: value::Preview,
    json_preview: Option<value::Preview>,
    preview_scroll: UniformListScrollHandle,
    preview_json: bool,
    console_output: String,
    console_scroll: gpui::ScrollHandle,
    error: Option<ViewError>,
    notice: Option<Message>,
    success_notice: Option<Message>,
    success_task: Option<Task<()>>,
    busy: Option<&'static str>,
    native_pending: bool,
    generation: usize,
    task: Option<Task<()>>,
    connection_watch: Option<Task<()>>,
    trust_task: Option<Task<()>>,
    key_scroll: UniformListScrollHandle,
    row_scroll: UniformListScrollHandle,
    bounds: Option<Bounds<Pixels>>,
    visible: bool,
    unread: bool,
    closed: bool,
}

impl EventEmitter<PaneEvent> for RedisView {}

impl Focusable for RedisView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RedisView {
    pub fn new(
        index: usize,
        title: String,
        launch: RedisLaunch,
        window_state: WindowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let RedisLaunch {
            config,
            secret,
            sentinel_secret,
            ssh,
        } = launch;
        let ssh_route = ssh.as_ref().map(|ssh| format!("{}:{}", ssh.host, ssh.port));
        let database_input = input(
            i18n_key::REDIS_DATABASE_NUMBER,
            &config.database.to_string(),
            cx,
        );
        let filter_input = input(i18n_key::REDIS_FILTER_KEYS_E_G_USER, "*", cx);
        let command_input = input(i18n_key::REDIS_REDIS_COMMAND_E_G_GET_USER_1, "", cx);
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&database_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.change_database(cx);
                }
            }),
            cx.subscribe_in(&filter_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.search(cx);
                }
            }),
            cx.subscribe_in(
                &command_input,
                window,
                |this, _, event, _, cx| match event {
                    InputEvent::Changed => {
                        this.command_revision = this.command_revision.wrapping_add(1);
                    }
                    InputEvent::Submit => this.execute(cx),
                    InputEvent::Rejected(message) => {
                        this.error = Some((*message).into());
                        cx.notify();
                    }
                },
            ),
            cx.on_focus_in(&focus, window, |this, _, cx| {
                this.unread = false;
                cx.emit(PaneEvent::Focused(this.index));
                cx.notify();
            }),
        ];
        let mut view = Self {
            ops: operations::Operations::new(cx),
            index,
            title,
            pane_close_visible: false,
            database: config.database,
            config,
            ssh_route,
            client: None,
            state: ConnectionState::Connecting,
            manual_disconnect: false,
            retryable_failure: true,
            window_state,
            focus,
            _subscriptions: subscriptions,
            database_input,
            filter_input,
            command_input,
            command_revision: 0,
            command_history: Vec::new(),
            command_history_index: None,
            command_history_draft: String::new(),
            keys: Vec::new(),
            scan_cursor: 0,
            scan_next: 0,
            scan_history: Vec::new(),
            scan_pattern: b"*".to_vec(),
            scanned: false,
            selected_key: None,
            info: None,
            detail_page: PageRequest::default(),
            detail_history: Vec::new(),
            selected_row: None,
            navigation: Navigation::Keys,
            editor: None,
            console_open: false,
            console_height: 180.,
            resizing_console: false,
            menu: None,
            menu_position: None,
            menu_index: 0,
            menu_scroll: gpui::ScrollHandle::new(),
            menu_return_focus: None,
            compact: true,
            viewport_size: (0., 0.),
            browser_open: false,
            browser_collapsed: false,
            browser_width: 244.,
            resizing_browser: false,
            raw_preview: value::Preview::default(),
            json_preview: None,
            preview_scroll: UniformListScrollHandle::default(),
            preview_json: true,
            console_output: String::new(),
            console_scroll: gpui::ScrollHandle::new(),
            error: None,
            notice: None,
            success_notice: None,
            success_task: None,
            busy: None,
            native_pending: false,
            generation: 0,
            task: None,
            connection_watch: None,
            trust_task: None,
            key_scroll: UniformListScrollHandle::default(),
            row_scroll: UniformListScrollHandle::default(),
            bounds: None,
            visible: true,
            unread: false,
            closed: false,
        };
        view.connect(secret, sentinel_secret, ssh, window, cx);
        view
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            ConnectionState::Connecting
                | ConnectionState::WaitingHostKey
                | ConnectionState::Connected
        )
    }

    pub fn has_unread(&self) -> bool {
        self.unread
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.editor.is_some()
    }

    pub fn status_text(&self) -> String {
        tr(match self.state {
            ConnectionState::Connecting => i18n_key::COMMON_CONNECTING_PROGRESS,
            ConnectionState::WaitingHostKey => i18n_key::TERMINAL_WAITING_FOR_HOST_CONFIRMATION,
            ConnectionState::Connected => i18n_key::COMMON_CONNECTED,
            ConnectionState::Disconnected => i18n_key::COMMON_DISCONNECTED,
            ConnectionState::Failed => i18n_key::REDIS_CONNECTION_FAILED,
        })
        .to_owned()
    }

    pub fn status_color(&self) -> u32 {
        match self.state {
            ConnectionState::Connecting | ConnectionState::WaitingHostKey => ui::WARNING,
            ConnectionState::Connected => ui::SUCCESS,
            ConnectionState::Disconnected => ui::MUTED,
            ConnectionState::Failed => ui::DANGER,
        }
    }

    pub fn status_details(&self) -> String {
        let mut details = format!(
            "{}:{} · DB {} · {}",
            self.config.host,
            self.config.port,
            self.database,
            if self.config.tls { "TLS" } else { "TCP" }
        );
        if let Some(host) = &self.ssh_route {
            details.push_str(" · ");
            details.push_str(&rustterm::trf!(i18n_key::REDIS_VIA_SSH_HOST, host = host));
        }
        details
    }

    pub fn diagnostic_state(&self) -> &'static str {
        match self.state {
            ConnectionState::Connecting => "connecting",
            ConnectionState::WaitingHostKey => "waiting_host_key",
            ConnectionState::Connected => "connected",
            ConnectionState::Disconnected => "disconnected",
            ConnectionState::Failed => "failed",
        }
    }

    pub fn set_pane_close_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.pane_close_visible != visible {
            self.pane_close_visible = visible;
            cx.notify();
        }
    }

    pub fn minimum_size(&self) -> (f32, f32) {
        (360., ui::PANE_HEADER_HEIGHT + 180.)
    }

    /// Workspace supplies the size before this entity renders. Prepaint bounds
    /// are reserved for pointer coordinates and must not drive responsive state.
    pub fn set_layout_size(&mut self, size: (f32, f32), cx: &mut Context<Self>) {
        let size = (size.0.max(0.), size.1.max(0.));
        if self.viewport_size != size {
            self.viewport_size = size;
            self.compact = size.0 < BROWSER_MIN_WIDTH;
            cx.notify();
        }
    }

    pub fn can_split(&self, axis: Axis) -> bool {
        let (width, height) = self.minimum_size();
        let (available_width, available_height) = self.viewport_size;
        match axis {
            Axis::Horizontal => available_width >= width * 2. + 6. && available_height >= height,
            Axis::Vertical => available_height >= height * 2. + 6. && available_width >= width,
        }
    }

    pub fn input_composing(&self, cx: &App) -> bool {
        self.ops.pattern.read(cx).is_composing()
            || self.ops.channel.read(cx).is_composing()
            || self.ops.payload.read(cx).is_composing()
            || self.database_input.read(cx).is_composing()
            || self.filter_input.read(cx).is_composing()
            || self.command_input.read(cx).is_composing()
            || self.editor.as_ref().is_some_and(|editor| {
                editor.name.read(cx).is_composing()
                    || editor.value.read(cx).is_composing()
                    || editor.extra.read(cx).is_composing()
            })
    }

    pub fn apply_settings(&mut self, _: &Settings, cx: &mut Context<Self>) {
        for input in [
            &self.ops.pattern,
            &self.ops.channel,
            &self.ops.payload,
            &self.database_input,
            &self.filter_input,
            &self.command_input,
        ] {
            input.update(cx, |_, cx| cx.notify());
        }
        if let Some(editor) = &self.editor {
            for input in [&editor.name, &editor.value, &editor.extra] {
                input.update(cx, |_, cx| cx.notify());
            }
        }
        cx.notify();
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible != self.visible {
            self.visible = visible;
            if !visible {
                self.clear_success();
            }
            if visible {
                self.unread = false;
            }
            cx.notify();
        }
    }

    pub fn save_position(&self, position: &mut rustterm::config::PanePosition) {
        position.database = Some(self.database);
        position.key_pattern = String::from_utf8(self.scan_pattern.clone())
            .ok()
            .filter(|p| p.len() <= 4096);
        position.selected_key = self.selected_key.clone().filter(|key| key.len() <= 4096);
    }
    pub fn restore_position(
        &mut self,
        position: &rustterm::config::PanePosition,
        cx: &mut Context<Self>,
    ) {
        self.database = if self.config.topology == rustterm::config::RedisTopology::Cluster {
            0
        } else {
            position
                .database
                .filter(|db| *db <= i32::MAX as u32)
                .unwrap_or(self.config.database)
        };
        self.database_input.update(cx, |input, cx| {
            input.set_value(self.database.to_string(), cx)
        });
        if let Some(pattern) = &position.key_pattern {
            self.scan_pattern = pattern.as_bytes().to_vec();
            self.filter_input
                .update(cx, |input, cx| input.set_value(pattern, cx));
        }
        self.selected_key = position.selected_key.clone();
        if self.state == ConnectionState::Connected {
            self.reload(self.selected_key.clone(), cx);
        }
        cx.notify();
    }
    pub fn may_auto_reconnect(&self) -> bool {
        !self.closed
            && !self.manual_disconnect
            && self.retryable_failure
            && matches!(
                self.state,
                ConnectionState::Failed | ConnectionState::Disconnected
            )
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        self.clear_success();
        self.disconnect(cx);
    }

    pub fn disconnect_or_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            self.error =
                Some(i18n_key::REDIS_SAVE_OR_CANCEL_THE_CURRENT_EDIT_BEFORE_NAVIGATING.into());
            cx.notify();
            return;
        }
        if self.is_running() {
            self.disconnect(cx);
        } else {
            cx.emit(PaneEvent::Reconnect(self.index));
        }
    }

    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            self.error =
                Some(i18n_key::REDIS_SAVE_OR_CANCEL_THE_CURRENT_EDIT_BEFORE_NAVIGATING.into());
            cx.notify();
            return;
        }
        self.ops.section = operations::Section::Keys;
        self.console_open = false;
        self.browser_open = true;
        self.browser_collapsed = false;
        self.filter_input.read(cx).focus(window);
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.clear_success();
        self.manual_disconnect = true;
        self.ops.disconnect();
        self.generation = self.generation.wrapping_add(1);
        if let Some(client) = self.client.take() {
            client.close();
        }
        self.task = None;
        self.connection_watch = None;
        self.trust_task = None;
        self.busy = None;
        self.set_editor_read_only(false, cx);
        self.state = ConnectionState::Disconnected;
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    fn watch_connection(&mut self, client: RedisClient, cx: &mut Context<Self>) {
        let generation = self.generation;
        let task = cx
            .background_executor()
            .spawn(async move { client.closed().await });
        self.connection_watch = Some(cx.spawn(async move |this, cx| {
            task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || generation != this.generation {
                    return;
                }
                if this.state == ConnectionState::Connected {
                    this.state = ConnectionState::Disconnected;
                    this.clear_success();
                    this.notice =
                        Some(i18n_key::REDIS_REDIS_CONNECTION_CLOSED_RECONNECT_TO_CONTINUE.into());
                    if !this.visible {
                        this.unread = true;
                    }
                    cx.emit(PaneEvent::Activity);
                    cx.notify();
                }
            });
        }));
    }

    fn available(&mut self, cx: &mut Context<Self>) -> bool {
        if self.closed || self.busy.is_some() || self.native_pending || !self.is_running() {
            return false;
        }
        if self.editor.is_some() {
            self.error =
                Some(i18n_key::REDIS_SAVE_OR_CANCEL_THE_CURRENT_EDIT_BEFORE_NAVIGATING.into());
            cx.notify();
            return false;
        }
        true
    }

    fn start(
        &mut self,
        label: &'static str,
        request: impl Future<Output = Result<Completion, RedisError>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.busy = Some(label);
        self.set_editor_read_only(true, cx);
        self.error = None;
        self.notice = None;
        self.clear_success();
        let generation = self.generation;
        let task = cx.background_executor().spawn(request);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || generation != this.generation {
                    return;
                }
                this.busy = None;
                this.set_editor_read_only(false, cx);
                match result {
                    Ok(result) => this.complete(result, cx),
                    Err(error) => this.fail(error, cx),
                }
                if !this.visible {
                    this.unread = true;
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn clear_success(&mut self) {
        self.success_task = None;
        self.success_notice = None;
    }

    fn show_success(&mut self, message: impl Into<Message>, cx: &mut Context<Self>) {
        self.clear_success();
        if self.closed || !self.visible || self.window_state.is_locked() {
            return;
        }
        self.success_notice = Some(message.into());
        self.success_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_secs(3)).await;
            let _ = this.update(cx, |this, cx| {
                this.success_notice = None;
                this.success_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn fail(&mut self, error: RedisError, cx: &mut Context<Self>) {
        self.clear_success();
        self.retryable_failure = matches!(
            error,
            RedisError::Connection
                | RedisError::Disconnected
                | RedisError::Timeout
                | RedisError::SshConnection
                | RedisError::SshTimeout
                | RedisError::Topology
        );
        self.window_state
            .record(error.diagnostic_code(), self.index);
        self.error = Some(ViewError::Backend(error));
        if self.client.as_ref().is_some_and(RedisClient::is_closed) {
            self.state = ConnectionState::Failed;
        }
        cx.notify();
    }

    fn complete(&mut self, result: Completion, cx: &mut Context<Self>) {
        match result {
            Completion::Scan {
                mut page,
                cursor,
                pattern,
                history,
                intent,
            } => {
                if let ScanIntent::Database(database) = intent {
                    self.database = database;
                    self.clear_detail();
                } else if pattern != self.scan_pattern {
                    self.clear_detail();
                }
                page.keys.sort();
                page.keys.dedup();
                self.keys = page.keys;
                self.scan_cursor = cursor;
                self.scan_next = page.next_cursor;
                self.scan_pattern = pattern;
                self.scan_history = history;
                self.scanned = true;
                if !matches!(intent, ScanIntent::Refresh) {
                    self.key_scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
            }
            Completion::Inspect {
                info,
                key,
                page,
                history,
            } => {
                let info = match info {
                    Ok(info) => info,
                    Err(error) => {
                        // A context menu opened for this pending selection must
                        // never fall back to actions on the previously read key.
                        self.menu = None;
                        self.menu_position = None;
                        self.menu_return_focus = None;
                        self.fail(error, cx);
                        return;
                    }
                };
                self.update_detail(key, info, page, history);
                self.browser_open = false;
            }
            Completion::Mutation {
                key,
                accepted,
                rejection,
            } => {
                if !accepted {
                    self.error = Some(rejection.into());
                    return;
                }
                self.editor = None;
                self.reload(key, cx);
                self.show_success(i18n_key::REDIS_CHANGES_SAVED, cx);
            }
            Completion::Reload {
                mut page,
                detail,
                detail_page,
            } => {
                page.keys.sort();
                page.keys.dedup();
                self.keys = page.keys;
                self.scan_next = page.next_cursor;
                self.scanned = true;
                if let Some((key, info)) = detail {
                    let history = if self.selected_key.as_ref() == Some(&key) {
                        self.detail_history.clone()
                    } else {
                        self.browser_open = false;
                        Vec::new()
                    };
                    self.update_detail(key, info, detail_page, history);
                } else {
                    self.clear_detail();
                }
            }
            Completion::Console { result, revision } => {
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        append_console(&mut self.console_output, &error.to_string());
                        self.console_scroll.scroll_to_bottom();
                        self.fail(error, cx);
                        return;
                    }
                };
                append_console(&mut self.console_output, &format_reply(&result));
                self.console_scroll.scroll_to_bottom();
                // Retyping the same command is still a new draft.
                if self.command_revision == revision {
                    self.command_input.update(cx, |input, cx| {
                        if !input.is_composing() {
                            input.clear(cx);
                        }
                    });
                }
                self.reload(self.selected_key.clone(), cx);
            }
        }
    }

    fn cache_preview(&mut self) {
        self.raw_preview = self
            .info
            .as_ref()
            .and_then(|info| match &info.data {
                KeyData::String(bytes) => Some(value::Preview::new(value::full_preview(bytes))),
                _ => None,
            })
            .unwrap_or_default();
        self.json_preview = self
            .info
            .as_ref()
            .and_then(|info| value::json_preview(info, &self.detail_page))
            .map(value::Preview::new);
    }

    fn clear_detail(&mut self) {
        self.selected_key = None;
        self.info = None;
        self.detail_page = PageRequest::default();
        self.detail_history.clear();
        self.selected_row = None;
        self.navigation = Navigation::Keys;
        self.cache_preview();
    }

    fn update_detail(
        &mut self,
        key: Vec<u8>,
        info: KeyInfo,
        page: PageRequest,
        history: Vec<PageRequest>,
    ) {
        let same_page = self.selected_key.as_ref() == Some(&key)
            && self.detail_page == page
            && self.info.as_ref().is_some_and(|old| old.kind == info.kind);
        self.selected_row = if same_page {
            self.info
                .as_ref()
                .and_then(|old| retained_row(&old.data, &info.data, self.selected_row?))
        } else {
            None
        };
        self.selected_key = Some(key);
        self.info = Some(info);
        self.detail_page = page;
        self.detail_history = history;
        self.cache_preview();
        if !same_page {
            self.preview_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
            self.row_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
    }

    /// A write is already committed before this read-only refresh starts. A
    /// refresh failure never leaves its editor available for an accidental retry.
    fn reload(&mut self, key: Option<Vec<u8>>, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let database = self.database;
        let cursor = self.scan_cursor;
        let pattern = self.scan_pattern.clone();
        let detail_page = if key.as_ref() == self.selected_key.as_ref() {
            self.detail_page.clone()
        } else {
            PageRequest::default()
        };
        self.start(
            i18n_key::REDIS_REFRESHING_REDIS_DATA,
            async move {
                let page = client.scan(database, cursor, &pattern).await?;
                let detail = if let Some(key) = key {
                    let info = client.inspect(database, &key, detail_page.clone()).await?;
                    Some((key, info))
                } else {
                    None
                };
                Ok(Completion::Reload {
                    page,
                    detail,
                    detail_page,
                })
            },
            cx,
        );
    }

    fn change_database(&mut self, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Ok(database) = self.database_input.read(cx).value().trim().parse::<u32>() else {
            self.error = Some(i18n_key::REDIS_ENTER_A_NONNEGATIVE_DATABASE_NUMBER.into());
            cx.notify();
            return;
        };
        if database == self.database {
            self.refresh_keys(cx);
        } else {
            self.scan(
                0,
                self.scan_pattern.clone(),
                Vec::new(),
                ScanIntent::Database(database),
                cx,
            );
        }
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let pattern = self.filter_input.read(cx).value().as_bytes().to_vec();
        if pattern == self.scan_pattern {
            self.refresh_keys(cx);
        } else {
            self.scan(0, pattern, Vec::new(), ScanIntent::Navigate, cx);
        }
    }

    pub(super) fn refresh_keys(&mut self, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        self.scan(
            self.scan_cursor,
            self.scan_pattern.clone(),
            self.scan_history.clone(),
            ScanIntent::Refresh,
            cx,
        );
    }

    fn scan(
        &mut self,
        cursor: u64,
        pattern: Vec<u8>,
        history: Vec<u64>,
        intent: ScanIntent,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let database = match intent {
            ScanIntent::Database(database) => database,
            _ => self.database,
        };
        self.start(
            i18n_key::REDIS_SCANNING_KEYS,
            async move {
                let page = client.scan(database, cursor, &pattern).await?;
                Ok(Completion::Scan {
                    page,
                    cursor,
                    pattern,
                    history,
                    intent,
                })
            },
            cx,
        );
    }

    fn page_keys(&mut self, next: bool, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let mut history = self.scan_history.clone();
        let cursor = if next {
            if self.scan_next == 0 {
                return;
            }
            history.push(self.scan_cursor);
            self.scan_next
        } else {
            let Some(cursor) = history.pop() else { return };
            cursor
        };
        self.scan(
            cursor,
            self.scan_pattern.clone(),
            history,
            ScanIntent::Navigate,
            cx,
        );
    }

    fn select_key(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(key) = self.keys.get(index).cloned() else {
            return;
        };
        self.focus.focus(window);
        self.navigation = Navigation::Keys;
        if self.selected_key.as_ref() == Some(&key) && self.info.is_some() {
            self.browser_open = false;
            cx.notify();
            return;
        }
        self.inspect(key, PageRequest::default(), Vec::new(), cx);
    }

    fn inspect(
        &mut self,
        key: Vec<u8>,
        page: PageRequest,
        history: Vec<PageRequest>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let database = self.database;
        self.start(
            i18n_key::REDIS_READING_VALUE,
            async move {
                let info = client.inspect(database, &key, page.clone()).await;
                Ok(Completion::Inspect {
                    info,
                    key,
                    page,
                    history,
                })
            },
            cx,
        );
    }

    fn refresh_value(&mut self, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        if let Some(key) = self.selected_key.clone() {
            self.inspect(
                key,
                self.detail_page.clone(),
                self.detail_history.clone(),
                cx,
            );
        }
    }

    fn page_value(&mut self, next: bool, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(key) = self.selected_key.clone() else {
            return;
        };
        let mut history = self.detail_history.clone();
        let page = if next {
            let Some(page) = self.info.as_ref().and_then(|info| info.next.clone()) else {
                return;
            };
            history.push(self.detail_page.clone());
            page
        } else {
            let Some(page) = history.pop() else { return };
            page
        };
        self.inspect(key, page, history, cx);
    }

    fn execute(&mut self, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let command = self.command_input.read(cx).value().to_owned();
        if command.trim().is_empty() {
            return;
        }
        append_console(
            &mut self.console_output,
            &format!("{} {} › {command}", tr(i18n_key::REDIS_DB), self.database),
        );
        self.console_scroll.scroll_to_bottom();
        self.remember_command(&command);
        let args = match rustterm::redis::parse_command(&command) {
            Ok(args) => args,
            Err(error) => {
                append_console(&mut self.console_output, &error.to_string());
                self.fail(error, cx);
                return;
            }
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let database = self.database;
        let revision = self.command_revision;
        self.start(
            i18n_key::REDIS_RUNNING_COMMAND,
            async move {
                let result = client.command(database, args).await;
                Ok(Completion::Console { result, revision })
            },
            cx,
        );
    }

    fn remember_command(&mut self, command: &str) {
        if self
            .command_history
            .last()
            .is_none_or(|last| last != command)
        {
            self.command_history.push(command.to_owned());
            let mut bytes = self.command_history.iter().map(String::len).sum::<usize>();
            while self.command_history.len() > MAX_COMMAND_HISTORY || bytes > MAX_CONSOLE_BYTES {
                bytes -= self.command_history.remove(0).len();
            }
        }
        self.command_history_index = None;
        self.command_history_draft.clear();
    }

    fn recall_command(&mut self, previous: bool, cx: &mut Context<Self>) {
        if self.command_history.is_empty() {
            return;
        }
        let next = match (self.command_history_index, previous) {
            (None, true) => {
                self.command_history_draft = self.command_input.read(cx).value().to_owned();
                Some(self.command_history.len() - 1)
            }
            (Some(index), true) => Some(index.saturating_sub(1)),
            (Some(index), false) if index + 1 < self.command_history.len() => Some(index + 1),
            (Some(_), false) => None,
            (None, false) => return,
        };
        self.command_history_index = next;
        let command = next
            .map(|index| &self.command_history[index])
            .unwrap_or(&self.command_history_draft);
        self.command_input
            .update(cx, |input, cx| input.set_value(command, cx));
        self.command_revision = self.command_revision.wrapping_add(1);
        cx.notify();
    }

    fn mutate(
        &mut self,
        args: Vec<Vec<u8>>,
        key: Option<Vec<u8>>,
        expected: ExpectedReply,
        rejection: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || self.native_pending || self.closed {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let database = self.database;
        self.start(
            i18n_key::WORKSPACE_SAVING_CHANGES,
            async move {
                let result = client.command(database, args).await?;
                let accepted = match expected {
                    ExpectedReply::Any => true,
                    ExpectedReply::Nonzero => {
                        matches!(result, RedisValue::Integer(value) if value != 0)
                    }
                    ExpectedReply::Present => !matches!(result, RedisValue::Nil),
                };
                Ok(Completion::Mutation {
                    key,
                    accepted,
                    rejection,
                })
            },
            cx,
        );
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.input_composing(cx) {
            return;
        }
        if self.menu.is_some() {
            self.menu_key_down(event, window, cx);
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.platform {
            match key {
                "t" => cx.emit(PaneEvent::NewTab),
                "w" => cx.emit(PaneEvent::Close(self.index)),
                "j" => cx.emit(PaneEvent::NextPane),
                "d" => cx.emit(PaneEvent::Split(Axis::Horizontal)),
                "e" => cx.emit(PaneEvent::Split(Axis::Vertical)),
                "enter" => cx.emit(PaneEvent::Zoom),
                "f" => self.open_search(window, cx),
                _ => return,
            }
        } else if modifiers.control && !modifiers.alt && !modifiers.platform && key == "tab" {
            cx.emit(PaneEvent::NextTab);
        } else if modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
            && key == "s"
            && self.editor.is_some()
        {
            self.save_editor(cx);
        } else if key == "f5" {
            if self.ops.section == operations::Section::Keys {
                self.refresh_keys(cx);
            } else if self.ops.section != operations::Section::PubSub {
                self.refresh_operations(cx);
            }
        } else if key == "enter"
            && self.ops.section == operations::Section::PubSub
            && self.ops.payload.read(cx).focus_handle().is_focused(window)
        {
            self.publish_message(cx);
        } else if key == "escape" && self.editor.is_some() && self.busy.is_none() {
            self.cancel_editor(window, cx);
        } else if self
            .command_input
            .read(cx)
            .focus_handle()
            .is_focused(window)
            && matches!(key, "up" | "down")
            && !modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
        {
            self.recall_command(key == "up", cx);
        } else if self.ops.section == operations::Section::Keys
            && self.focus.is_focused(window)
            && matches!(key, "enter" | "f2" | "delete")
            && !modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
        {
            match (self.navigation, key) {
                (Navigation::Values, "enter" | "f2") if self.entry_editable() => {
                    self.edit_value(false, window, cx);
                }
                (Navigation::Values, "delete") if self.entry_removable() => {
                    self.delete_row(window, cx);
                }
                (Navigation::Keys, "f2") => self.rename_key(window, cx),
                (Navigation::Keys, "delete") => self.delete_key(window, cx),
                _ => return,
            }
        } else if self.ops.section == operations::Section::Keys
            && self.focus.is_focused(window)
            && matches!(key, "up" | "down" | "home" | "end")
            && !modifiers.control
            && !modifiers.alt
        {
            if !self.available(cx) {
                return;
            }
            let (count, current) = match self.navigation {
                Navigation::Keys => (
                    self.keys.len(),
                    self.selected_key
                        .as_ref()
                        .and_then(|key| self.keys.iter().position(|item| item == key)),
                ),
                Navigation::Values => (
                    self.info
                        .as_ref()
                        .map_or(0, |info| render::row_count(&info.data)),
                    self.selected_row,
                ),
            };
            if count == 0 {
                return;
            }
            let index = match key {
                "up" => current.unwrap_or(1).saturating_sub(1),
                "down" => current.map_or(0, |index| (index + 1).min(count - 1)),
                "home" => 0,
                _ => count - 1,
            };
            match self.navigation {
                Navigation::Keys => {
                    self.key_scroll
                        .scroll_to_item(index, ScrollStrategy::Center);
                    self.select_key(index, window, cx);
                }
                Navigation::Values => {
                    self.selected_row = Some(index);
                    self.row_scroll
                        .scroll_to_item(index, ScrollStrategy::Center);
                    cx.notify();
                }
            }
        } else {
            return;
        }
        cx.stop_propagation();
        window.prevent_default();
    }
}

impl Drop for RedisView {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.close();
        }
    }
}

fn retained_row(previous: &KeyData, current: &KeyData, row: usize) -> Option<usize> {
    match (previous, current) {
        (KeyData::Hash(before), KeyData::Hash(after))
        | (KeyData::ZSet(before), KeyData::ZSet(after)) => {
            let identity = &before.get(row)?.0;
            after.iter().position(|(key, _)| key == identity)
        }
        (KeyData::Set(before), KeyData::Set(after)) => {
            let member = before.get(row)?;
            after.iter().position(|value| value == member)
        }
        (KeyData::Stream(before), KeyData::Stream(after)) => {
            let id = &before.get(row)?.id;
            after.iter().position(|entry| &entry.id == id)
        }
        (KeyData::List(before), KeyData::List(after)) => {
            // Lists have no stable member ID. A shifted or changed value must
            // not inherit a selection used by the next edit action.
            (before.get(row)? == after.get(row)?).then_some(row)
        }
        _ => None,
    }
}

fn append_console(output: &mut String, entry: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(entry);
    if output.len() > MAX_CONSOLE_BYTES {
        let mut start = output.len() - MAX_CONSOLE_BYTES;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        output.drain(..start);
    }
}

fn input(placeholder: &'static str, value: &str, cx: &mut Context<RedisView>) -> Entity<TextInput> {
    cx.new(|cx| {
        let mut input = TextInput::new(placeholder, false, cx);
        input.set_compact(true, cx);
        input.set_value(value, cx);
        input
    })
}

fn format_reply(value: &RedisValue) -> String {
    fn append(value: &RedisValue, depth: usize, output: &mut String) {
        if output.len() >= MAX_CONSOLE_BYTES || depth > 12 {
            return;
        }
        match value {
            RedisValue::Nil => output.push_str("(nil)"),
            RedisValue::Integer(number) => output.push_str(&number.to_string()),
            RedisValue::Bytes(bytes) => output.push_str(&value::preview(bytes)),
            RedisValue::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    if output.len() >= MAX_CONSOLE_BYTES {
                        output.push_str("\n…");
                        break;
                    }
                    output.push_str(&format!("\n{}{}) ", "  ".repeat(depth), index + 1));
                    append(value, depth + 1, output);
                }
            }
        }
    }
    let mut output = String::new();
    append(value, 0, &mut output);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_tracks_collection_identity_after_reordering() {
        let before = KeyData::Hash(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let after = KeyData::Hash(vec![
            (b"b".to_vec(), b"changed".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
        ]);
        assert_eq!(retained_row(&before, &after, 1), Some(0));
        assert_eq!(retained_row(&before, &KeyData::Hash(Vec::new()), 1), None);
        assert_eq!(retained_row(&before, &KeyData::Missing, 1), None);
    }

    #[test]
    fn list_selection_does_not_follow_shifted_or_replaced_members() {
        let before = KeyData::List(vec![b"a".to_vec(), b"b".to_vec()]);
        let shifted = KeyData::List(vec![b"new".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(retained_row(&before, &before, 1), Some(1));
        assert_eq!(retained_row(&before, &shifted, 1), None);
        assert_eq!(retained_row(&before, &KeyData::List(Vec::new()), 1), None);
    }

    #[test]
    fn console_retains_recent_output_with_a_utf8_safe_limit() {
        let mut output = String::new();
        append_console(&mut output, "DB 0 › GET key");
        append_console(&mut output, "value");
        assert_eq!(output, "DB 0 › GET key\nvalue");
        append_console(&mut output, &"中".repeat(MAX_CONSOLE_BYTES));
        assert!(output.len() <= MAX_CONSOLE_BYTES);
        assert!(output.chars().all(|ch| ch == '中'));
        append_console(&mut output, "DB 0 › PING\nPONG");
        assert!(output.len() <= MAX_CONSOLE_BYTES);
        assert!(output.ends_with("DB 0 › PING\nPONG"));
    }
}
