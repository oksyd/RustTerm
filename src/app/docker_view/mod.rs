mod render;
use crate::app::{
    interaction::WindowState,
    resource_monitor::{MonitorInterest, ResourceMonitor, ResourceRegistry},
    resource_view::{ResourceEvent, ResourceView},
    text_input::{InputEvent, TextInput},
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Subscription, Task, Timer, Window, prelude::*,
};
use rustterm::{
    config::Uuid,
    docker::{
        Action, Container, ContainerDetails, DockerClient, Endpoint, EngineVersion, LogBuffer,
        Shell,
    },
    i18n::{messages as key, tr},
};
use std::time::{Duration, Instant};
#[derive(Clone)]
pub enum DockerEvent {
    Close,
    Console {
        client: DockerClient,
        id: String,
        name: String,
        shell: Shell,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Details,
    Logs,
    Stats,
}
#[derive(Clone)]
enum ListRow {
    Project(String, usize),
    Container(usize),
}
struct ContainerMonitor {
    id: String,
    model: Entity<ResourceMonitor>,
    view: Entity<ResourceView>,
    interest: MonitorInterest,
    _subscription: Subscription,
}
pub struct DockerView {
    session: Uuid,
    client: DockerClient,
    window_state: WindowState,
    focus: FocusHandle,
    active: bool,
    collecting: bool,
    closed: bool,
    failed: bool,
    search: Entity<TextInput>,
    log_search: Entity<TextInput>,
    socket: Entity<TextInput>,
    show_socket: bool,
    containers: Vec<Container>,
    rows: Vec<ListRow>,
    version: Option<EngineVersion>,
    selected: Option<String>,
    details: Option<ContainerDetails>,
    page: Page,
    registry: ResourceRegistry,
    stats: Option<ContainerMonitor>,
    logs: LogBuffer,
    log_paused: bool,
    log_follow: bool,
    log_rows: std::sync::Arc<Vec<rustterm::docker::LogLine>>,
    log_width: f32,
    log_horizontal: gpui::ScrollHandle,
    log_ended: bool,
    log_error: Option<String>,
    list_scroll: gpui::ListState,
    log_scroll: gpui::UniformListScrollHandle,
    shell: Shell,
    error: Option<String>,
    notice: Option<&'static str>,
    last_list: Option<Instant>,
    poll: Option<Task<()>>,
    request: Option<Task<()>>,
    log_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    idle_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<DockerEvent> for DockerView {}
impl DockerView {
    pub fn new(
        session: Uuid,
        client: DockerClient,
        registry: ResourceRegistry,
        window_state: WindowState,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| TextInput::new(key::DOCKER_SEARCH, false, cx));
        let log_search = cx.new(|cx| TextInput::new(key::DOCKER_LOG_SEARCH, false, cx));
        let socket = cx.new(|cx| TextInput::new("/var/run/docker.sock", false, cx));
        let subscriptions = vec![
            cx.subscribe(&socket, |this, _, event, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.connect(cx);
                }
            }),
            cx.subscribe(&search, |this, _, event, cx| {
                if matches!(event, InputEvent::Changed) {
                    this.filter(cx);
                    cx.notify();
                }
            }),
            cx.subscribe(&log_search, |this, _, _, cx| {
                this.filter_logs(cx);
                cx.notify();
            }),
        ];
        Self {
            session,
            client,
            window_state,
            focus: cx.focus_handle(),
            active: false,
            collecting: false,
            closed: false,
            failed: false,
            search,
            log_search,
            socket,
            show_socket: false,
            containers: Vec::new(),
            rows: Vec::new(),
            version: None,
            selected: None,
            details: None,
            page: Page::Details,
            stats: None,
            registry,
            logs: LogBuffer::default(),
            log_paused: false,
            log_follow: true,
            log_rows: Default::default(),
            log_width: 400.,
            log_horizontal: gpui::ScrollHandle::new(),
            log_ended: false,
            log_error: None,
            list_scroll: gpui::ListState::new(0, gpui::ListAlignment::Top, gpui::px(120.)),
            log_scroll: Default::default(),
            shell: Shell::Sh,
            error: None,
            notice: None,
            last_list: None,
            poll: None,
            request: None,
            log_task: None,
            action_task: None,
            idle_task: None,
            _subscriptions: subscriptions,
        }
    }
    pub fn session_id(&self) -> Uuid {
        self.session
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }
    pub fn input_composing(&self, cx: &App) -> bool {
        [&self.search, &self.log_search, &self.socket]
            .iter()
            .any(|input| input.read(cx).is_composing())
    }
    fn visible(&self) -> bool {
        self.active
            && !self.closed
            && self.window_state.visibility.is_visible()
            && !self.window_state.is_locked()
    }
    fn interactive(&self, cx: &App) -> bool {
        self.visible() && !self.window_state.prompts.is_busy() && !self.input_composing(cx)
    }
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.closed || self.active == active {
            return;
        }
        self.active = active;
        self.poll = None;
        if active {
            self.tick(cx);
            self.poll = Some(cx.spawn(async move |this, cx| {
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                        break;
                    }
                }
            }));
        } else {
            self.pause_collection(cx);
        }
        cx.notify();
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.set_active(false, cx);
        self.closed = true;
        self.action_task = None;
    }
    fn pause_collection(&mut self, cx: &mut Context<Self>) {
        self.request = None;
        self.log_task = None;
        self.log_ended = false;
        self.stats = None;
        self.collecting = false;
        let client = self.client.clone();
        self.idle_task = Some(cx.spawn(async move |_, _| {
            client.release_idle().await;
        }));
    }
    fn tick(&mut self, cx: &mut Context<Self>) {
        if !self.visible() {
            if self.collecting {
                self.pause_collection(cx);
                cx.notify();
            }
            return;
        }
        if !self.collecting {
            self.collecting = true;
            self.last_list = None;
            cx.notify();
        }
        if !self.client.is_connected() {
            self.error = Some(tr(key::DOCKER_DISCONNECTED).into());
            self.failed = true;
            self.log_task = None;
            return;
        }
        if self.action_task.is_none() && self.request.is_none() && !self.failed {
            let list = self
                .last_list
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(10));
            if list {
                self.fetch(list, cx);
            }
        }
        self.sync_stats(cx);
        if self.page == Page::Logs
            && !self.log_paused
            && !self.log_ended
            && self.log_task.is_none()
            && self.details.is_some()
        {
            self.start_logs(cx);
        }
    }
    fn sync_stats(&mut self, cx: &mut Context<Self>) {
        let active = self.visible()
            && self.page == Page::Stats
            && self.details.as_ref().is_some_and(|d| d.state.running)
            && !self.failed;
        if active
            && let Some(details) = &self.details
            && self
                .stats
                .as_ref()
                .is_none_or(|stats| stats.id != details.id)
        {
            let id = details.id.clone();
            let model = self.registry.container(
                self.client.clone(),
                id.clone(),
                self.window_state.clone(),
                cx,
            );
            let interest = model.read(cx).subscribe();
            let view = cx.new(|cx| ResourceView::new(model.clone(), self.window_state.clone(), cx));
            let subscription = cx.subscribe(&view, |this, _, event, cx| {
                if matches!(event, ResourceEvent::Close) && this.interactive(cx) {
                    cx.emit(DockerEvent::Close);
                }
            });
            self.stats = Some(ContainerMonitor {
                id,
                model,
                view,
                interest,
                _subscription: subscription,
            });
        }

        if let Some(stats) = &self.stats {
            stats.interest.set(active, false);
        }
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.interactive(cx) || self.action_task.is_some() {
            return;
        }
        self.request = None;
        self.failed = false;
        self.error = None;
        if self.page == Page::Logs {
            self.log_task = None;
            self.log_ended = false;
        }
        if self.page == Page::Stats
            && let Some(stats) = &self.stats
        {
            stats.model.update(cx, |monitor, cx| monitor.retry(cx));
        }
        self.fetch(true, cx);
    }
    fn filter(&mut self, cx: &App) {
        let offset = self.list_scroll.logical_scroll_top();
        self.rows.clear();
        let search = self.search.read(cx).value();
        let mut group = None::<String>;
        for (index, container) in self
            .containers
            .iter()
            .enumerate()
            .filter(|(_, c)| c.matches(search))
        {
            let project = container.project();
            if group.as_deref() != Some(project) {
                group = Some(project.into());
                self.rows.push(ListRow::Project(project.into(), 0));
            }
            self.rows.push(ListRow::Container(index));
        }
        let mut count = 0;
        for row in self.rows.iter_mut().rev() {
            match row {
                ListRow::Container(_) => count += 1,
                ListRow::Project(_, n) => {
                    *n = count;
                    count = 0;
                }
            }
        }
        self.list_scroll.reset(self.rows.len());
        self.list_scroll.scroll_to(offset);
    }
    fn fetch(&mut self, list: bool, cx: &mut Context<Self>) {
        if !self.visible() {
            return;
        }
        if list {
            self.last_list = Some(Instant::now());
        }
        let client = self.client.clone();
        let selected = self.selected.clone();
        let version = self.version.is_none();
        self.request = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let version = if version {
                    Some(client.version().await?)
                } else {
                    None
                };
                let containers = if list {
                    Some(client.containers().await?)
                } else {
                    None
                };
                let exists = selected.as_ref().is_some_and(|id| {
                    containers
                        .as_ref()
                        .is_none_or(|items| items.iter().any(|c| &c.id == id))
                });
                let details = if exists && let Some(id) = &selected {
                    Some(client.inspect(id).await?)
                } else {
                    None
                };
                Ok::<_, rustterm::docker::Error>((version, containers, details))
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.request = None;
                match result {
                    Ok((version, containers, details)) => {
                        if let Some(version) = version {
                            this.version = Some(version);
                        }
                        if let Some(containers) = containers {
                            this.containers = containers;
                            this.filter(cx);
                        }
                        if this.selected == selected {
                            this.details = details;
                            if this.selected.is_some() && this.details.is_none() {
                                this.selected = None;
                                this.log_task = None;
                                this.page = Page::Details;
                            }
                        }
                        this.error = None;
                        this.failed = false;
                    }
                    Err(error) => {
                        this.error = Some(error.to_string());
                        this.failed = true;
                    }
                }
                this.sync_stats(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if !self.interactive(cx) {
            return;
        }
        self.request = None;
        self.log_task = None;
        self.selected = id;
        self.details = None;
        self.stats = None;
        self.page = Page::Details;
        self.logs.clear();
        self.filter_logs(cx);
        self.log_ended = false;
        self.log_error = None;
        self.error = None;
        self.notice = None;
        if self.selected.is_some() {
            self.fetch(false, cx);
        }
        cx.notify();
    }
    fn page(&mut self, page: Page, cx: &mut Context<Self>) {
        if !self.interactive(cx) || self.page == page {
            return;
        }
        self.page = page;
        self.log_task = None;
        self.log_ended = false;
        self.stats = None;
        self.tick(cx);
        cx.notify();
    }
    fn connect(&mut self, cx: &mut Context<Self>) {
        if !self.interactive(cx) || self.action_task.is_some() {
            return;
        }
        match Endpoint::parse(self.socket.read(cx).value()) {
            Ok(endpoint) => {
                self.pause_collection(cx);
                self.client = self.client.with_endpoint(endpoint);
                self.version = None;
                self.containers.clear();
                self.rows.clear();
                self.selected = None;
                self.details = None;
                self.logs.clear();
                self.filter_logs(cx);
                self.page = Page::Details;
                self.failed = false;
                self.error = None;
                self.notice = None;
                self.show_socket = false;
                self.tick(cx);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }
    fn start_logs(&mut self, cx: &mut Context<Self>) {
        let Some(details) = &self.details else { return };
        let id = details.id.clone();
        let tty = details.config.tty;
        let client = self.client.clone();
        self.logs.clear();
        self.filter_logs(cx);
        self.log_error = None;
        self.log_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let mut stream = client.logs(&id, tty).await?;
                while let Some(first) = stream.next().await {
                    let mut batch = vec![first?];
                    let mut bytes = batch[0].1.len();
                    let start = Instant::now();
                    let mut ended = false;
                    let mut failure = None;
                    while bytes < 64 * 1024 && start.elapsed() < Duration::from_millis(100) {
                        tokio::select! {
                            next = stream.next() => match next {
                                Some(Ok(chunk)) => { bytes += chunk.1.len(); batch.push(chunk); }
                                Some(Err(error)) => { failure = Some(error); break; }
                                None => { ended = true; break; }
                            },
                            _ = Timer::after(Duration::from_millis(100).saturating_sub(start.elapsed())) => break,
                        }
                    }
                    if this.update(cx, |this, cx| {
                        for (stderr, data) in batch { this.logs.append(stderr, &data); }
                        this.filter_logs(cx);
                        cx.notify();
                    }).is_err() { return Ok(()); }
                    if let Some(error) = failure { return Err(error); }
                    if ended { break; }
                }
                Ok::<_, rustterm::docker::Error>(())
            }.await;
            let _ = this.update(cx, |this, cx| {
                this.logs.finish();
                this.filter_logs(cx);
                this.log_task = None;
                this.log_ended = true;
                this.log_error = result.err().map(|error| error.to_string());
                cx.notify();
            });
        }));
    }
    fn filter_logs(&mut self, cx: &App) {
        let filter = self.log_search.read(cx).value().to_lowercase();
        let rows: Vec<_> = self
            .logs
            .lines()
            .filter(|line| line.text.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        self.log_width = rows
            .iter()
            .map(|line| line.text.chars().count())
            .max()
            .unwrap_or(0)
            .max(50) as f32
            * 8.
            + 24.;
        if self.log_follow && !rows.is_empty() {
            self.log_scroll
                .scroll_to_item(rows.len() - 1, gpui::ScrollStrategy::Bottom);
        }
        self.log_rows = std::sync::Arc::new(rows);
    }

    fn toggle_logs(&mut self, cx: &mut Context<Self>) {
        if !self.interactive(cx) {
            return;
        }
        self.log_paused = !self.log_paused;
        self.log_task = None;
        self.log_ended = false;
        self.logs.finish();
        self.filter_logs(cx);
        self.tick(cx);
        cx.notify();
    }
    fn action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive(cx) || self.action_task.is_some() {
            return;
        }
        let Some(details) = &self.details else { return };
        let id = details.id.clone();
        let name = details.name.trim_start_matches('/').to_owned();
        if action == Action::Start {
            self.execute(id, action, cx);
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &rustterm::trf!(
                key::DOCKER_CONFIRM_ACTION,
                action = tr(action_label(action)),
                name = name
            ),
            Some(tr(key::DOCKER_CONFIRM_DETAIL)),
            &[tr(key::DOCKER_CANCEL), tr(action_label(action))],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = this.update_in(cx, |this, window, cx| {
                let valid = permit.is_valid();
                drop(permit);
                if confirmed && valid && this.selected.as_ref() == Some(&id) && this.interactive(cx)
                {
                    this.execute(id, action, cx);
                }
                this.focus.focus(window);
            });
        })
        .detach();
    }
    fn execute(&mut self, id: String, action: Action, cx: &mut Context<Self>) {
        if self.action_task.is_some() {
            return;
        }
        self.error = None;
        self.notice = None;
        self.request = None;
        let client = self.client.clone();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = client.action(&id, action).await;
            let _ = this.update(cx, |this, cx| {
                this.action_task = None;
                match result {
                    Ok(()) => {
                        this.notice = Some(key::DOCKER_OPERATION_DONE);
                        if action != Action::Stop {
                            this.log_task = None;
                            this.log_ended = false;
                        }
                        this.failed = false;
                        this.last_list = None;
                        this.tick(cx);
                    }
                    Err(error) => {
                        this.error = Some(error.to_string());
                        this.failed = true;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn console(&mut self, cx: &mut Context<Self>) {
        if !self.interactive(cx) {
            return;
        }
        let Some(details) = &self.details else { return };
        if !details.state.running {
            return;
        }
        cx.emit(DockerEvent::Console {
            client: self.client.clone(),
            id: details.id.clone(),
            name: details.name.trim_start_matches('/').into(),
            shell: self.shell,
        });
    }
}
fn action_label(action: Action) -> &'static str {
    match action {
        Action::Start => key::DOCKER_START,
        Action::Stop => key::DOCKER_STOP,
        Action::Restart => key::DOCKER_RESTART,
    }
}
