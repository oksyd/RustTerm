use super::*;
use rustterm::database::processlist::{MysqlMonitor, Process, Snapshot};
use std::time::{Duration, Instant};
mod render;

pub(super) struct MonitorState {
    client: Option<MysqlMonitor>,
    task: Option<Task<()>>,
    poll: Option<Task<()>>,
    request: u64,
    snapshot: Option<Snapshot>,
    rows: Vec<usize>,
    search: Entity<TextInput>,
    database: Entity<TextInput>,
    hide_sleep: bool,
    refresh_secs: u64,
    sort: (usize, bool),
    selected: Option<u64>,
    preview: Option<SqlPreview>,
    scroll: gpui::UniformListScrollHandle,
    horizontal: gpui::ScrollHandle,
    last_attempt: Option<Instant>,
    updated: Option<String>,
    error: Option<Message>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}
struct SqlPreview {
    sql: String,
    chunks: Vec<std::ops::Range<usize>>,
    scroll: gpui::ListState,
}
#[derive(Clone)]
pub(in crate::app::database_view) struct MonitorDraft {
    search: String,
    database: String,
    hide_sleep: bool,
    refresh_secs: u64,
    sort: (usize, bool),
}
impl MonitorState {
    pub(super) fn composing(&self, cx: &App) -> bool {
        self.search.read(cx).is_composing() || self.database.read(cx).is_composing()
    }
    pub(super) fn new(cx: &mut Context<DatabaseView>) -> Self {
        let search = cx.new(|cx| TextInput::new(i18n_key::DATABASE_SESSIONS_SEARCH, false, cx));
        let database =
            cx.new(|cx| TextInput::new(i18n_key::DATABASE_SESSIONS_DATABASE_FILTER, false, cx));
        let subscriptions = [&search, &database]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event, cx| {
                    if matches!(event, InputEvent::Changed)
                        && let Some(monitor) = &mut this.monitor
                    {
                        monitor.filter(cx);
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            client: None,
            task: None,
            poll: None,
            request: 0,
            snapshot: None,
            rows: Vec::new(),
            search,
            database,
            hide_sleep: true,
            refresh_secs: 0,
            sort: (5, true),
            selected: None,
            preview: None,
            scroll: Default::default(),
            horizontal: gpui::ScrollHandle::new(),
            last_attempt: None,
            updated: None,
            error: None,
            busy: false,
            _subscriptions: subscriptions,
        }
    }
    pub(in crate::app::database_view) fn draft(&self, cx: &App) -> MonitorDraft {
        MonitorDraft {
            search: self.search.read(cx).value().into(),
            database: self.database.read(cx).value().into(),
            hide_sleep: self.hide_sleep,
            refresh_secs: self.refresh_secs,
            sort: self.sort,
        }
    }
    pub(in crate::app::database_view) fn restore(
        &mut self,
        draft: MonitorDraft,
        cx: &mut Context<DatabaseView>,
    ) {
        self.search
            .update(cx, |input, cx| input.set_value(draft.search, cx));
        self.database
            .update(cx, |input, cx| input.set_value(draft.database, cx));
        self.hide_sleep = draft.hide_sleep;
        self.refresh_secs = draft.refresh_secs;
        self.sort = draft.sort;
    }
    fn filter(&mut self, cx: &App) {
        self.rows = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                filtered_rows(
                    &snapshot.processes,
                    self.search.read(cx).value(),
                    self.database.read(cx).value(),
                    self.hide_sleep,
                    self.sort,
                )
            })
            .unwrap_or_default();
        if let Some(id) = self.selected
            && !self
                .rows
                .iter()
                .any(|&i| self.snapshot.as_ref().unwrap().processes[i].id == id)
        {
            self.selected = None;
            self.preview = None;
        }
    }
    fn select(&mut self, id: u64) {
        self.selected = Some(id);
        let sql = self
            .snapshot
            .as_ref()
            .and_then(|s| s.processes.iter().find(|p| p.id == id))
            .and_then(|p| p.sql.as_deref());
        if sql == self.preview.as_ref().map(|v| v.sql.as_str()) {
            return;
        }
        self.preview = sql.map(|sql| {
            let chunks = values::value_chunks(sql);
            SqlPreview {
                scroll: gpui::ListState::new(
                    chunks.len(),
                    gpui::ListAlignment::Top,
                    gpui::px(100.),
                ),
                sql: sql.into(),
                chunks,
            }
        });
    }
}

impl DatabaseView {
    fn monitor_visible(&self) -> bool {
        self.page == WorkspacePage::Sessions
            && self.visible
            && !self.closed
            && self.window_state.visibility.is_visible()
            && !self.window_state.is_locked()
    }
    fn monitor_connected(&self) -> bool {
        self.interactive()
            && self
                .client
                .as_ref()
                .is_some_and(DatabaseClient::is_connected)
    }
    pub(super) fn open_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() || self.monitor.is_none() {
            return;
        }
        self.page = WorkspacePage::Sessions;
        self.object_menu = None;
        self.resize_drag = None;
        self.focus.focus(window);
        self.start_monitor_poll(cx);
        self.refresh_sessions(cx);
        cx.notify();
    }
    pub(super) fn pause_monitor(&mut self) {
        if let Some(monitor) = &mut self.monitor {
            monitor.request = monitor.request.wrapping_add(1);
            monitor.task = None;
            monitor.poll = None;
            monitor.busy = false;
        }
    }
    pub(super) fn close_monitor(&mut self) {
        self.pause_monitor();
        if let Some(monitor) = &mut self.monitor
            && let Some(client) = monitor.client.take()
        {
            client.close();
        }
    }
    pub(super) fn start_monitor_poll(&mut self, cx: &mut Context<Self>) {
        if self.monitor.is_none() {
            return;
        }
        let poll = cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(500)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.closed || this.page != WorkspacePage::Sessions {
                            return false;
                        }
                        let visible = this.monitor_visible();
                        let connected = this.monitor_connected();
                        let Some(monitor) = &mut this.monitor else {
                            return false;
                        };
                        if !visible && monitor.busy {
                            monitor.request = monitor.request.wrapping_add(1);
                            monitor.task = None;
                            monitor.busy = false;
                            cx.notify();
                        }
                        if refresh_due(
                            monitor.last_attempt,
                            monitor.refresh_secs,
                            monitor.busy,
                            visible && connected,
                        ) {
                            this.refresh_sessions(cx);
                        }
                        true
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        });
        self.monitor.as_mut().unwrap().poll = Some(poll);
    }
    pub(super) fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        if !self.monitor_visible() || !self.monitor_connected() {
            return;
        }
        let Some(monitor) = &mut self.monitor else {
            return;
        };
        if monitor.busy {
            return;
        }
        if monitor.client.is_none() {
            match self.client.as_ref().unwrap().mysql_monitor() {
                Ok(client) => monitor.client = Some(client),
                Err(error) => {
                    monitor.error = Some(Message::from_error(error));
                    cx.notify();
                    return;
                }
            }
        }
        monitor.busy = true;
        monitor.error = None;
        monitor.last_attempt = Some(Instant::now());
        monitor.request = monitor.request.wrapping_add(1);
        let request = monitor.request;
        let client = monitor.client.as_ref().unwrap().clone();
        let generation = self.generation;
        let task = cx
            .background_executor()
            .spawn(async move { client.snapshot().await });
        monitor.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                let Some(monitor) = &mut this.monitor else {
                    return;
                };
                if monitor.request != request {
                    return;
                }
                monitor.busy = false;
                match result {
                    Ok(snapshot) => {
                        monitor.snapshot = Some(snapshot);
                        monitor.updated = Some(chrono::Local::now().format("%H:%M:%S").to_string());
                        monitor.filter(cx);
                        if let Some(id) = monitor.selected {
                            monitor.select(id);
                        }
                    }
                    Err(error) => {
                        monitor.error = Some(Message::from_error(error));
                        monitor.refresh_secs = 0;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    pub(super) fn monitor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.page != WorkspacePage::Sessions {
            return false;
        }
        let Some(monitor) = &mut self.monitor else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "f5" {
            self.refresh_sessions(cx);
            return true;
        }
        if modifiers.control && !modifiers.alt && !modifiers.platform && key == "f" {
            monitor.search.read(cx).focus(window);
            return true;
        }
        if !self.focus.is_focused(window) {
            return false;
        }
        if key == "escape" && monitor.selected.is_some() {
            monitor.selected = None;
            monitor.preview = None;
            cx.notify();
            return true;
        }
        if modifiers.control && key == "c" {
            if let Some(preview) = &monitor.preview {
                cx.write_to_clipboard(ClipboardItem::new_string(preview.sql.clone()));
            }
            return true;
        }
        if !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && matches!(key, "up" | "down")
            && !monitor.rows.is_empty()
        {
            let processes = &monitor.snapshot.as_ref().unwrap().processes;
            let current = monitor
                .selected
                .and_then(|id| monitor.rows.iter().position(|&i| processes[i].id == id));
            let next = match (current, key) {
                (Some(i), "up") => i.saturating_sub(1),
                (Some(i), _) => (i + 1).min(monitor.rows.len() - 1),
                _ => 0,
            };
            let id = processes[monitor.rows[next]].id;
            monitor.select(id);
            monitor
                .scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Center);
            cx.notify();
            return true;
        }
        false
    }
}
fn refresh_due(last: Option<Instant>, seconds: u64, busy: bool, visible: bool) -> bool {
    visible
        && !busy
        && seconds > 0
        && last.is_none_or(|last| last.elapsed() >= Duration::from_secs(seconds))
}
fn filtered_rows(
    processes: &[Process],
    search: &str,
    database: &str,
    hide_sleep: bool,
    sort: (usize, bool),
) -> Vec<usize> {
    let search = search.trim().to_lowercase();
    let database = database.trim().to_lowercase();
    let mut rows: Vec<_> = processes
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            (!hide_sleep || !p.command.eq_ignore_ascii_case("sleep"))
                && (database.is_empty()
                    || p.database
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&database))
                && (search.is_empty()
                    || [
                        p.user.as_str(),
                        p.host.as_str(),
                        p.command.as_str(),
                        p.state.as_deref().unwrap_or(""),
                        p.sql.as_deref().unwrap_or(""),
                    ]
                    .iter()
                    .any(|s| s.to_lowercase().contains(&search))
                    || p.id.to_string().contains(&search))
        })
        .map(|(i, _)| i)
        .collect();
    rows.sort_by(|&a, &b| {
        let (a, b) = (&processes[a], &processes[b]);
        let order = match sort.0 {
            0 => a.id.cmp(&b.id),
            1 => a.user.cmp(&b.user),
            2 => a.host.cmp(&b.host),
            3 => a.database.cmp(&b.database),
            4 => a.command.cmp(&b.command),
            5 => a.seconds.cmp(&b.seconds),
            6 => a.state.cmp(&b.state),
            _ => a.sql.cmp(&b.sql),
        };
        (if sort.1 { order.reverse() } else { order }).then_with(|| a.id.cmp(&b.id))
    });
    rows
}
#[cfg(test)]
mod tests {
    use super::*;
    fn process(id: u64, seconds: u64, command: &str, database: Option<&str>) -> Process {
        Process {
            id,
            user: "app".into(),
            host: "host:123".into(),
            database: database.map(str::to_owned),
            command: command.into(),
            seconds: Some(seconds),
            state: None,
            sql: Some("SELECT 中文".into()),
        }
    }
    #[test]
    fn filtering_and_numeric_sort_preserve_ids_and_null_databases() {
        let p = vec![
            process(3, 2, "Query", Some("analytics")),
            process(2, 100, "Query", None),
            process(1, 10, "Sleep", Some("analytics")),
        ];
        assert_eq!(filtered_rows(&p, "", "", true, (5, true)), [1, 0]);
        assert_eq!(
            filtered_rows(&p, "中文", "ANALYTICS", false, (5, false)),
            [0, 2]
        );
        assert_eq!(
            filtered_rows(&p, "host:123", "", false, (0, false)),
            [2, 1, 0]
        );
    }
    #[test]
    fn refresh_policy_never_polls_manual_hidden_or_busy_views() {
        let old = Some(Instant::now() - Duration::from_secs(10));
        assert!(refresh_due(old, 5, false, true));
        assert!(!refresh_due(old, 0, false, true));
        assert!(!refresh_due(old, 5, true, true));
        assert!(!refresh_due(old, 5, false, false));
        assert!(!refresh_due(Some(Instant::now()), 5, false, true));
    }
}
