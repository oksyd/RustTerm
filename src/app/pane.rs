//! Workspace lifecycle shared by terminal, database and remote desktop panes. Data operations
//! remain on their concrete views rather than pretending every connection is a PTY.
use super::{
    database_view::DatabaseView, layout::Axis, rdp_view::RdpView, redis_view::RedisView,
    terminal_view::TerminalView,
};
use gpui::{AnyElement, App, Entity, FocusHandle, IntoElement};
use rustterm::config::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTool {
    Files,
    Commands,
    Resources,
}

#[derive(Clone, Debug)]
pub enum PaneEvent {
    Focused(usize),
    Reconnect(usize),
    Close(usize),
    NewTab,
    Split(Axis),
    NextTab,
    NextPane,
    Files,
    Commands,
    Resources,
    Zoom,
    Activity,
    CommandFinished {
        index: usize,
        run: rustterm::config::Uuid,
        command: rustterm::terminal::ShellCommand,
    },
    DirectoryChanged(usize, String),
    RdpCertificateTrusted {
        index: usize,
        fingerprint: String,
    },
    DatabaseChanged {
        index: usize,
        database: String,
    },
}

pub enum Pane {
    Terminal(Entity<TerminalView>),
    Redis(Entity<RedisView>),
    Database(Entity<DatabaseView>),
    Rdp(Entity<RdpView>),
}

impl Pane {
    pub fn set_pane_close_visible(&self, visible: bool, cx: &mut App) {
        match self {
            Self::Terminal(_) => {}
            Self::Redis(view) => {
                view.update(cx, |view, cx| view.set_pane_close_visible(visible, cx));
            }
            Self::Database(view) => {
                view.update(cx, |view, cx| view.set_pane_close_visible(visible, cx));
            }
            Self::Rdp(view) => {
                view.update(cx, |view, cx| view.set_pane_close_visible(visible, cx));
            }
        }
    }

    pub fn element(&self, size: (f32, f32), cx: &mut App) -> AnyElement {
        match self {
            Self::Terminal(view) => view.clone().into_any_element(),
            Self::Rdp(view) => {
                view.update(cx, |view, cx| view.set_layout_size(size, cx));
                view.clone().into_any_element()
            }
            Self::Redis(view) => {
                view.update(cx, |view, cx| view.set_layout_size(size, cx));
                view.clone().into_any_element()
            }
            Self::Database(view) => {
                view.update(cx, |view, cx| view.set_layout_size(size, cx));
                view.clone().into_any_element()
            }
        }
    }

    pub fn focus_handle<'a>(&'a self, cx: &'a App) -> &'a FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(),
            Self::Redis(view) => view.read(cx).focus_handle(),
            Self::Database(view) => view.read(cx).focus_handle(),
            Self::Rdp(view) => view.read(cx).focus_handle(),
        }
    }

    pub fn is_running(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).is_running(),
            Self::Redis(view) => view.read(cx).is_running(),
            Self::Database(view) => view.read(cx).is_running(),
            Self::Rdp(view) => view.read(cx).is_running(),
        }
    }

    pub fn has_unread(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).has_unread(),
            Self::Redis(view) => view.read(cx).has_unread(),
            Self::Database(view) => view.read(cx).has_unread(),
            Self::Rdp(view) => view.read(cx).has_unread(),
        }
    }

    pub fn has_unsaved_changes(&self, cx: &App) -> bool {
        match self {
            Self::Redis(view) => view.read(cx).has_unsaved_changes(),
            Self::Database(view) => view.read(cx).has_unsaved_changes(),
            _ => false,
        }
    }

    pub fn status_text(&self, cx: &App) -> String {
        match self {
            Self::Terminal(view) => view.read(cx).status_text(),
            Self::Redis(view) => view.read(cx).status_text(),
            Self::Database(view) => view.read(cx).status_text(),
            Self::Rdp(view) => view.read(cx).status_text(),
        }
    }

    pub fn status_color(&self, cx: &App) -> u32 {
        match self {
            Self::Terminal(view) => view.read(cx).status_color(),
            Self::Redis(view) => view.read(cx).status_color(),
            Self::Database(view) => view.read(cx).status_color(),
            Self::Rdp(view) => view.read(cx).status_color(),
        }
    }

    pub fn status_details(&self, cx: &App) -> String {
        match self {
            Self::Terminal(view) => view.read(cx).status_details(),
            Self::Redis(view) => view.read(cx).status_details(),
            Self::Database(view) => view.read(cx).status_details(),
            Self::Rdp(view) => view.read(cx).status_details(),
        }
    }

    pub fn diagnostic_state(&self, cx: &App) -> &'static str {
        match self {
            Self::Terminal(view) => view.read(cx).diagnostic_state(),
            Self::Redis(view) => view.read(cx).diagnostic_state(),
            Self::Database(view) => view.read(cx).diagnostic_state(),
            Self::Rdp(view) => view.read(cx).diagnostic_state(),
        }
    }

    pub fn input_composing(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).input_composing(cx),
            Self::Redis(view) => view.read(cx).input_composing(cx),
            Self::Database(view) => view.read(cx).input_composing(cx),
            Self::Rdp(view) => view.read(cx).input_composing(cx),
        }
    }

    pub fn minimum_size(&self, cx: &App) -> (f32, f32) {
        match self {
            Self::Terminal(view) => view.read(cx).minimum_size(),
            Self::Redis(view) => view.read(cx).minimum_size(),
            Self::Database(view) => view.read(cx).minimum_size(),
            Self::Rdp(view) => view.read(cx).minimum_size(),
        }
    }

    /// Reserve enough width to show the database browser before spending space
    /// on the global connection library. A narrower pane remains usable.
    pub fn preferred_size(&self, cx: &App) -> (f32, f32) {
        let minimum = self.minimum_size(cx);
        match self {
            Self::Redis(_) => (super::redis_view::BROWSER_MIN_WIDTH, minimum.1),
            Self::Database(_) => (super::database_view::BROWSER_MIN_WIDTH, minimum.1),
            Self::Terminal(_) | Self::Rdp(_) => minimum,
        }
    }

    pub fn can_split(&self, axis: Axis, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).can_split(axis),
            Self::Redis(view) => view.read(cx).can_split(axis),
            Self::Database(view) => view.read(cx).can_split(axis),
            Self::Rdp(view) => view.read(cx).can_split(axis),
        }
    }

    pub fn set_visible(&self, visible: bool, cx: &mut App) {
        match self {
            Self::Terminal(view) => view.update(cx, |view, cx| view.set_visible(visible, cx)),
            Self::Redis(view) => view.update(cx, |view, cx| view.set_visible(visible, cx)),
            Self::Database(view) => view.update(cx, |view, cx| view.set_visible(visible, cx)),
            Self::Rdp(view) => view.update(cx, |view, cx| view.set_visible(visible, cx)),
        }
    }

    pub fn set_locked(&self, locked: bool, cx: &mut App) {
        if let Self::Rdp(view) = self {
            view.update(cx, |view, cx| view.set_locked(locked, cx));
        }
    }

    pub fn apply_settings(&self, settings: &Settings, cx: &mut App) {
        match self {
            Self::Terminal(view) => view.update(cx, |view, cx| view.apply_settings(settings, cx)),
            Self::Redis(view) => view.update(cx, |view, cx| view.apply_settings(settings, cx)),
            Self::Database(view) => view.update(cx, |view, cx| view.apply_settings(settings, cx)),
            Self::Rdp(view) => view.update(cx, |view, cx| view.apply_settings(settings, cx)),
        }
    }

    pub fn close(&self, cx: &mut App) {
        match self {
            Self::Terminal(view) => view.update(cx, |view, cx| view.close(cx)),
            Self::Redis(view) => view.update(cx, |view, cx| view.close(cx)),
            Self::Database(view) => view.update(cx, |view, cx| view.close(cx)),
            Self::Rdp(view) => view.update(cx, |view, cx| view.close(cx)),
        }
    }
}
