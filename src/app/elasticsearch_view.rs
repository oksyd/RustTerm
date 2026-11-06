mod render;
mod response;
use response::PreparedResponse;

use super::{
    interaction::WindowState,
    layout::Axis,
    pane::PaneEvent,
    text_input::{InputEvent, TextInput},
    ui,
};
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, KeyDownEvent,
    PromptLevel, Subscription, Task, Timer, Window,
};
use rustterm::{
    config::{ElasticsearchConfig, Secret, Settings},
    elasticsearch::{DEFAULT_QUERY, ElasticsearchClient, Index, Request},
    i18n::{Message, messages as msg, tr},
    session::{HostKeyChallenge, SshOptions},
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub struct ElasticsearchLaunch {
    pub config: ElasticsearchConfig,
    pub secret: Secret,
    pub ssh: Option<SshOptions>,
}
#[derive(Clone)]
struct Query {
    index: Option<String>,
    body: String,
}
#[derive(Clone)]
pub struct Draft {
    query: Query,
    history: VecDeque<Query>,
}

pub struct ElasticsearchView {
    index: usize,
    title: String,
    config: ElasticsearchConfig,
    window_state: WindowState,
    focus: FocusHandle,
    query: Entity<TextInput>,
    filter: Entity<TextInput>,
    client: Option<ElasticsearchClient>,
    task: Option<Task<()>>,
    trust_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    connecting: bool,
    waiting_host_key: bool,
    error: Option<Message>,
    indices: Vec<Index>,
    indices_truncated: bool,
    selected: Option<String>,
    history: VecDeque<Query>,
    history_open: bool,
    browser_open: bool,
    json: Arc<String>,
    lines: Vec<gpui::SharedString>,
    response_status: Option<(u16, u128)>,
    visible: bool,
    unread: bool,
    closed: bool,
    pane_close_visible: bool,
    viewport_size: (f32, f32),
}

impl EventEmitter<PaneEvent> for ElasticsearchView {}
impl ElasticsearchView {
    pub fn new(
        index: usize,
        title: String,
        launch: ElasticsearchLaunch,
        window_state: WindowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut input = TextInput::new(msg::ES_QUERY, false, cx);
            input.set_literal(true, cx);
            input.set_value(DEFAULT_QUERY, cx);
            input
        });
        let filter = cx.new(|cx| {
            let mut input = TextInput::new(msg::ES_FILTER, false, cx);
            input.set_compact(true, cx);
            input
        });
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus_in(&focus, window, |this, _, cx| {
                cx.emit(PaneEvent::Focused(this.index))
            }),
            cx.subscribe(&filter, |_, _, _, cx| cx.notify()),
            cx.subscribe(&query, |this, _, event, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.run(false, cx);
                }
                if let InputEvent::Rejected(message) = event {
                    this.error = Some((*message).into());
                    cx.notify();
                }
            }),
        ];
        let mut view = Self {
            index,
            title,
            config: launch.config,
            window_state,
            focus,
            query,
            filter,
            client: None,
            task: None,
            trust_task: None,
            _subscriptions: subscriptions,
            connecting: true,
            waiting_host_key: false,
            error: None,
            indices: Vec::new(),
            indices_truncated: false,
            selected: None,
            history: VecDeque::new(),
            history_open: false,
            browser_open: true,
            json: Arc::new(String::new()),
            lines: Vec::new(),
            response_status: None,
            visible: true,
            unread: false,
            closed: false,
            pane_close_visible: true,
            viewport_size: (800., 600.),
        };
        let config = view.config.clone();
        let ssh = launch.ssh.map(|ssh| (ssh, view.trust_channel(window, cx)));
        let task = cx
            .background_executor()
            .spawn(async move { ElasticsearchClient::connect(config, launch.secret, ssh).await });
        view.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed {
                    if let Ok(client) = result {
                        client.close();
                    }
                    return;
                }
                this.task = None;
                this.connecting = false;
                this.waiting_host_key = false;
                match result {
                    Ok(client) => {
                        this.client = Some(client);
                        this.refresh(cx);
                    }
                    Err(error) => this.error = Some(Message::from_error(error)),
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        view
    }

    fn available(&self) -> bool {
        !self.closed
            && self.client.is_some()
            && self.task.is_none()
            && !self.window_state.is_locked()
            && !self.window_state.prompts.is_blocked()
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
    pub fn is_running(&self) -> bool {
        !self.closed && (self.connecting || self.client.is_some())
    }
    pub fn has_unread(&self) -> bool {
        self.unread
    }
    pub fn has_unsaved_changes(&self, cx: &App) -> bool {
        let body = self.query.read(cx).value();
        body != DEFAULT_QUERY
            && !body.trim().is_empty()
            && !self.history.iter().any(|query| query.body == body)
    }
    pub fn diagnostic_state(&self) -> &'static str {
        if self.connecting {
            "connecting"
        } else if self.client.is_some() {
            "connected"
        } else if self.error.is_some() {
            "failed"
        } else {
            "disconnected"
        }
    }
    pub fn status_text(&self) -> String {
        tr(if self.waiting_host_key {
            msg::TERMINAL_WAITING_FOR_HOST_CONFIRMATION
        } else if self.connecting {
            msg::COMMON_CONNECTING_PROGRESS
        } else if self.client.is_some() {
            msg::COMMON_CONNECTED
        } else {
            msg::COMMON_DISCONNECTED
        })
        .into()
    }
    pub fn status_color(&self) -> u32 {
        if self.error.is_some() {
            ui::colors().danger
        } else if self.client.is_some() {
            ui::colors().success
        } else {
            ui::colors().muted
        }
    }
    pub fn status_details(&self) -> String {
        format!("Elasticsearch · {}", self.config.endpoint)
    }
    pub fn input_composing(&self, cx: &App) -> bool {
        self.query.read(cx).is_composing() || self.filter.read(cx).is_composing()
    }
    pub fn minimum_size(&self) -> (f32, f32) {
        (400., 400.)
    }
    pub fn set_layout_size(&mut self, size: (f32, f32), cx: &mut Context<Self>) {
        if self.viewport_size != size {
            self.viewport_size = size;
            cx.notify();
        }
    }
    pub fn can_split(&self, axis: Axis) -> bool {
        let (w, h) = self.minimum_size();
        match axis {
            Axis::Horizontal => self.viewport_size.0 >= 2. * w + 6. && self.viewport_size.1 >= h,
            Axis::Vertical => self.viewport_size.1 >= 2. * h + 6. && self.viewport_size.0 >= w,
        }
    }
    pub fn set_pane_close_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.pane_close_visible != visible {
            self.pane_close_visible = visible;
            cx.notify();
        }
    }
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.unread = false;
        }
        cx.notify();
    }
    pub fn apply_settings(&mut self, _: &Settings, cx: &mut Context<Self>) {
        cx.notify();
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        self.cancel(cx);
        if let Some(client) = self.client.take() {
            client.close();
        }
        self.trust_task = None;
        cx.notify();
    }
    pub fn disconnect_or_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            return;
        }
        if self.is_running() {
            self.cancel(cx);
            if let Some(client) = self.client.take() {
                client.close();
            }
            self.error = None;
            cx.emit(PaneEvent::Activity);
            cx.notify();
        } else {
            cx.emit(PaneEvent::Reconnect(self.index));
        }
    }
    pub fn draft(&self, cx: &App) -> Draft {
        Draft {
            query: Query {
                index: self.selected.clone(),
                body: self.query.read(cx).value().to_owned(),
            },
            history: self.history.clone(),
        }
    }
    pub fn restore_draft(&mut self, draft: Draft, cx: &mut Context<Self>) {
        self.selected = draft.query.index;
        self.query
            .update(cx, |input, cx| input.set_value(draft.query.body, cx));
        self.history = draft.history;
    }
    pub fn copy_json(&self, cx: &mut Context<Self>) {
        if !self.window_state.is_locked() && !self.json.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.json.as_ref().clone()));
        }
    }
    pub fn focus_query(&self, window: &mut Window, cx: &App) {
        self.query.read(cx).focus(window);
    }
    pub fn paste_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked()
            || self.window_state.prompts.is_blocked()
            || self.input_composing(cx)
        {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.query
                .update(cx, |input, cx| input.insert_text(&text, cx));
            if self.viewport_size.0 < 760. {
                self.browser_open = false;
            }
            self.focus_query(window, cx);
            cx.notify();
        }
    }
    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser_open = true;
        self.history_open = false;
        self.filter.read(cx).focus(window);
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        let client = self.client.clone().unwrap();
        self.error = None;
        let task = cx
            .background_executor()
            .spawn(async move { client.indices().await });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed {
                    return;
                }
                this.task = None;
                match result {
                    Ok(list) => {
                        this.indices = list.indices;
                        this.indices_truncated = list.truncated;
                    }
                    Err(error) => this.error = Some(Message::from_error(error)),
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn run(&mut self, mapping: bool, cx: &mut Context<Self>) {
        if !self.available() || self.input_composing(cx) {
            return;
        }
        let query = Query {
            index: self.selected.clone(),
            body: self.query.read(cx).value().to_owned(),
        };
        if !mapping && let Err(error) = rustterm::elasticsearch::validate_query(&query.body) {
            self.error = Some(Message::from_error(error));
            cx.notify();
            return;
        }
        let request = if mapping {
            Request::Mapping(query.index.clone())
        } else {
            Request::Search {
                index: query.index.clone(),
                body: query.body.clone(),
            }
        };
        if !mapping {
            self.history.push_front(query);
            while self.history.len() > 50
                || self
                    .history
                    .iter()
                    .map(|query| query.body.len())
                    .sum::<usize>()
                    > 256 * 1024
            {
                self.history.pop_back();
            }
        }
        let client = self.client.clone().unwrap();
        self.error = None;
        let task = cx.background_executor().spawn(async move {
            client
                .request(request)
                .await
                .and_then(PreparedResponse::new)
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed {
                    return;
                }
                this.task = None;
                match result {
                    Ok(response) => this.show_response(response),
                    Err(error) => this.error = Some(Message::from_error(error)),
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

    fn show_response(&mut self, response: PreparedResponse) {
        self.response_status = Some(response.status);
        self.lines = response.lines;
        self.json = response.json;
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        self.connecting = false;
        self.waiting_host_key = false;
        self.trust_task = None;
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.input_composing(cx) {
            return;
        }
        if event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && event.keystroke.key == "enter"
        {
            self.run(false, cx);
            window.prevent_default();
            cx.stop_propagation();
        } else if event.keystroke.key == "escape" && self.task.is_some() {
            self.cancel(cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn trust_channel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> mpsc::Sender<HostKeyChallenge> {
        let (sender, mut receiver) = mpsc::channel::<HostKeyChallenge>(1);
        self.trust_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(challenge) = receiver.recv().await {
                let permit = loop {
                    if challenge.reply.is_closed() {
                        return;
                    }
                    let state = this
                        .update(cx, |this, cx| {
                            if this.closed || !this.connecting {
                                return None;
                            }
                            if !this.waiting_host_key {
                                this.waiting_host_key = true;
                                cx.emit(PaneEvent::Activity);
                                cx.notify();
                            }
                            Some(
                                if this.visible
                                    && this.window_state.visibility.is_visible()
                                    && !this.window_state.is_locked()
                                {
                                    this.window_state.prompts.acquire()
                                } else {
                                    None
                                },
                            )
                        })
                        .ok()
                        .flatten();
                    match state {
                        None => return,
                        Some(Some(permit)) => break permit,
                        Some(None) => {
                            Timer::after(Duration::from_millis(100)).await;
                        }
                    }
                };
                let detail = rustterm::trf!(
                    msg::ES_TUNNEL_FINGERPRINT,
                    host = challenge.host,
                    port = challenge.port,
                    algorithm = challenge.algorithm,
                    fingerprint = challenge.fingerprint
                );
                let response = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        tr(msg::REDIS_VERIFY_THE_SSH_TUNNEL_HOST_KEY),
                        Some(&detail),
                        &[tr(msg::COMMON_CANCEL), tr(msg::TERMINAL_TRUST_AND_SAVE)],
                        cx,
                    )
                });
                let Ok(response) = response else {
                    return;
                };
                // The native prompt owns its permit until answered, even if this pane closes.
                let this = this.clone();
                cx.spawn(async move |cx| {
                    let trusted = response.await == Ok(1);
                    let current = this
                        .update(cx, |this, cx| {
                            this.waiting_host_key = false;
                            cx.notify();
                            !this.closed && this.connecting && permit.is_valid()
                        })
                        .unwrap_or(false);
                    let _ = challenge.reply.send(trusted && current);
                    drop(permit);
                })
                .detach();
            }
        }));
        sender
    }
}
impl Drop for ElasticsearchView {
    fn drop(&mut self) {
        if let Some(client) = &self.client {
            client.close();
        }
    }
}
