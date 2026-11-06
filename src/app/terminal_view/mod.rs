//! Dedicated terminal canvas: GPUI entities are panes, never individual cells.
//! The actor supplies an immutable visible snapshot; paint never locks the grid.

mod activity;
mod chrome;
mod clipboard;
mod completion;
mod context_menu;
mod input;
mod lifecycle;
mod paint;
mod render;
mod shell;
mod snapshot;
mod transfer;
pub use render::TerminalHeader;

use crate::app::{
    interaction::WindowState,
    pane::{PaneEvent, SessionTool},
    resize::ResizeScheduler,
    text_input::TextInput,
};
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Font, Pixels, Point, ScrollHandle,
    Subscription, Task,
};
use rustterm::config::{Connection, Profile, Settings};
use rustterm::session::{ResizeReceipt, Session, ZmodemStatus};
use rustterm::terminal::Snapshot;
use std::{ops::Range, sync::Arc};

const PADDING: f32 = 8.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalKind {
    Local,
    Ssh,
    Telnet,
}

pub struct TerminalView {
    runs: Vec<Session>,
    viewed_run: usize,
    generation: usize,
    settings: Settings,
    unread: bool,
    output_activity: activity::OutputActivity,
    observed_view_revision: u64,
    observed_insert_ready: bool,
    observed_directory: Option<(rustterm::config::Uuid, String)>,
    trust_prompt: bool,
    zmodem_prompt: Option<(rustterm::config::Uuid, u64)>,
    zmodem: Option<ZmodemStatus>,
    window_state: WindowState,
    selection_drag: Option<input::SelectionDrag>,
    selection_pointer: Option<Point<Pixels>>,
    search_input: Entity<TextInput>,
    search_open: bool,
    search_case: bool,
    show_final_alternate: bool,
    _search_subscription: Subscription,
    snapshot: Option<Arc<Snapshot>>,
    snapshot_task: Option<Task<()>>,
    command_task: Option<Task<()>>,
    focus: FocusHandle,
    menu_focus: FocusHandle,
    menu_open: bool,
    context_menu_position: Option<Point<Pixels>>,
    menu_index: usize,
    menu_scroll: ScrollHandle,
    _menu_blur: Subscription,
    index: usize,
    shell: String,
    display_name: String,
    target: Option<String>,
    local_shell: Option<String>,
    kind: TerminalKind,
    active_tool: Option<SessionTool>,
    header_external: bool,
    compact_header: bool,
    external_header: Option<Entity<TerminalHeader>>,
    manually_disconnected: bool,
    previous_directory: Option<String>,
    font: Font,
    metrics_font_size: Option<f32>,
    font_size: f32,
    initial_font_size: f32,
    cell_width: Pixels,
    line_height: Pixels,
    bounds: Option<Bounds<Pixels>>,
    resize: ResizeScheduler,
    resize_receipt: Option<ResizeReceipt>,
    requested_revision: u64,
    state: String,
    error: Option<rustterm::i18n::Message>,
    dismissed_feedback: Option<String>,
    visible: bool,
    surface_visible: bool,
    preedit: String,
    preedit_selection: Range<usize>,
    paste_pending: bool,
    copy_task: Option<Task<()>>,
    primary_copy_task: Option<Task<()>>,
    mouse_button: Option<u8>,
    last_mouse: (u16, u16),
    wheel: input::WheelState,
    poll_wake: Arc<tokio::sync::Notify>,
    _poll: Task<()>,
    _focus: Subscription,
    _blur: Subscription,
    _activation: Subscription,
}

impl TerminalView {
    pub fn set_active_tool(&mut self, tool: Option<SessionTool>, cx: &mut Context<Self>) {
        if self.active_tool != tool {
            self.active_tool = tool;
            cx.notify();
        }
    }

    pub fn set_header_external(&mut self, external: bool, cx: &mut Context<Self>) {
        if self.header_external != external {
            self.header_external = external;
            cx.notify();
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn resource_session(&self) -> Option<&Session> {
        (self.kind == TerminalKind::Ssh && self.viewed_run + 1 == self.runs.len())
            .then(|| self.runs.last())
            .flatten()
    }

    pub fn has_unread(&self) -> bool {
        self.unread
    }

    pub fn set_title(&mut self, title: String, cx: &mut Context<Self>) {
        if self.shell != title {
            self.shell = title.clone();
            self.display_name = title;
            cx.notify();
        }
    }

    /// Connection metadata belongs to the pane chrome, never to terminal output.
    pub fn set_profile(&mut self, profile: &Profile, cx: &mut Context<Self>) {
        self.display_name = profile.name.clone();
        match &profile.connection {
            Connection::Ssh(ssh) => {
                self.kind = TerminalKind::Ssh;
                let host = if ssh.host.contains(':') && !ssh.host.starts_with('[') {
                    format!("[{}]", ssh.host)
                } else {
                    ssh.host.clone()
                };
                self.target = Some(if ssh.port == 22 {
                    format!("{}@{host}", ssh.username)
                } else {
                    format!("{}@{host}:{}", ssh.username, ssh.port)
                });
                self.local_shell = None;
                self.shell = format!(
                    "{} · {}@{}:{}",
                    profile.name, ssh.username, ssh.host, ssh.port
                );
            }
            Connection::Telnet(telnet) => {
                self.kind = TerminalKind::Telnet;
                let host = if telnet.host.contains(':') && !telnet.host.starts_with('[') {
                    format!("[{}]", telnet.host)
                } else {
                    telnet.host.clone()
                };
                self.target = Some(format!("{host}:{}", telnet.port));
                self.local_shell = None;
                self.shell = format!("{} · {host}:{}", profile.name, telnet.port);
            }
            Connection::Local(local) => {
                self.kind = TerminalKind::Local;
                self.target = local.cwd.as_ref().map(|path| path.display().to_string());
                self.local_shell = Some(local.shell.clone());
                self.shell = profile.name.clone();
            }
            Connection::Elasticsearch(_)
            | Connection::Redis(_)
            | Connection::Rdp(_)
            | Connection::Mysql(_)
            | Connection::Postgres(_)
            | Connection::Sqlite(_) => return,
        }
        cx.notify();
    }

    pub fn composing(&self) -> bool {
        !self.preedit.is_empty()
    }

    pub fn input_composing(&self, cx: &App) -> bool {
        self.composing() || self.search_input.read(cx).is_composing()
    }

    pub fn metrics(&self) -> (usize, u64) {
        (
            self.runs.len(),
            self.runs.iter().map(Session::snapshot_count).sum(),
        )
    }
}

impl EventEmitter<PaneEvent> for TerminalView {}
