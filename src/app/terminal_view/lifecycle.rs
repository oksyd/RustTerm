use super::paint::terminal_font;
use super::{TerminalKind, TerminalView};
use crate::app::{
    interaction::WindowState,
    pane::PaneEvent,
    resize::ResizeScheduler,
    text_input::{InputEvent, TextInput},
    ui,
};
use gpui::{Context, PromptLevel, ScrollHandle, Timer, Window, prelude::*, px};
use rustterm::config::Settings;
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use rustterm::session::{Command, FailureKind, ResizeReceipt, Session, SessionState};
use std::time::{Duration, Instant};

type SessionChanges = Option<(rustterm::config::Uuid, tokio::sync::watch::Receiver<u64>)>;

fn observe_session(changes: &mut SessionChanges, session: Option<&Session>) {
    if changes.as_ref().map(|(id, _)| *id) != session.map(Session::id) {
        *changes = session.map(|session| (session.id(), session.subscribe_changes()));
    }
    if let Some((_, receiver)) = changes {
        // Mark before reading state; changes arriving during the refresh must wake us again.
        receiver.borrow_and_update();
    }
}

async fn session_changed(changes: &mut SessionChanges) {
    if let Some((_, receiver)) = changes {
        let _ = receiver.changed().await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn poll_intervals(visible: bool, focused: bool, continuous: bool) -> (Duration, Duration) {
    let frame = Duration::from_millis(if !visible {
        250
    } else if focused {
        16
    } else {
        34
    });
    let maintenance = if visible && continuous {
        frame
    } else {
        Duration::from_millis(250)
    };
    (frame, maintenance)
}

impl TerminalView {
    pub fn new(
        index: usize,
        shell: String,
        session: anyhow::Result<Session>,
        settings: &Settings,
        window_state: WindowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (session, error) = match session {
            Ok(session) => (Some(session), None),
            Err(error) => (
                None,
                Some(rustterm::i18n::Message::with_arg(
                    i18n_key::TERMINAL_COULD_NOT_START_ERROR,
                    "error",
                    format!("{error:#}"),
                )),
            ),
        };
        let snapshot = session.as_ref().map(Session::snapshot);
        let font = terminal_font(&settings.font_family);
        let font_size = settings.font_size;
        if let Some(session) = &session {
            let _ = session.send(Command::SetHighlight(settings.highlight));
            let _ = session.send(Command::SetCommandOutputCache(
                settings.cache_command_output,
            ));
            let _ = session.send(Command::SetTheme(settings.theme.clone()));
            let _ = session.send(Command::SetHistoryBudget {
                lines: settings.history_lines,
                bytes: settings.history_bytes,
            });
        }
        let search_input = cx.new(|cx| {
            TextInput::new(
                i18n_key::TERMINAL_SEARCH_CURRENT_CONNECTION_OUTPUT,
                false,
                cx,
            )
        });
        search_input.update(cx, |input, cx| input.set_compact(true, cx));
        let search_subscription = cx.subscribe(&search_input, |this, _input, event, cx| {
            match event {
                InputEvent::Changed if this.search_open => {
                    let query = this.search_input.read(cx).value().to_owned();
                    this.send(
                        Command::Search {
                            query,
                            case_sensitive: this.search_case,
                        },
                        cx,
                    );
                }
                InputEvent::Submit => this.send(Command::NextMatch { backwards: false }, cx),
                InputEvent::Rejected(message) => this.error = Some((*message).into()),
                InputEvent::Changed => {}
            }
            cx.notify();
        });
        let focus = cx.focus_handle();
        let on_focus = cx.on_focus(&focus, window, |this, window, cx| {
            this.poll_wake.notify_one();
            cx.emit(PaneEvent::Focused(this.index));
            if this.visible && window.is_window_active() {
                this.report_focus(true);
            }
            cx.notify();
        });
        let on_blur = cx.on_blur(&focus, window, |this: &mut Self, _, cx| {
            this.release_input();
            this.report_focus(false);
            cx.notify();
        });
        let activation = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            this.poll_wake.notify_one();
            let active = window.is_window_active();
            if !active {
                this.release_input();
            }
            if this.visible && this.focus.is_focused(window) {
                this.report_focus(active);
            }
            cx.notify();
        });
        let menu_focus = cx.focus_handle();
        let menu_blur = cx.on_blur(&menu_focus, window, |this: &mut Self, _, cx| {
            if this.menu_open {
                this.menu_open = false;
                this.context_menu_position = None;
                cx.notify();
            }
        });
        let poll_wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let wake = poll_wake.clone();
        let poll = cx.spawn_in(window, async move |this, cx| {
            let mut viewed_changes = None;
            let mut live_changes = None;
            loop {
                let Ok(Ok((frame, maintenance))) = cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        this.prune_evicted(window, cx);
                        let viewed = this.runs.get(this.viewed_run);
                        observe_session(&mut viewed_changes, viewed);
                        observe_session(
                            &mut live_changes,
                            this.runs.last().filter(|live| {
                                viewed.is_none_or(|viewed| viewed.id() != live.id())
                            }),
                        );
                        this.poll(window, cx);
                        poll_intervals(
                            this.surface_visible,
                            this.focus.is_focused(window),
                            this.selection_scroll_direction() != 0
                                || (!this.show_final_alternate
                                    && this.runs.get(this.viewed_run).is_some()
                                    && this.resize.is_pending()),
                        )
                    })
                }) else {
                    break;
                };
                let next_frame = Instant::now() + frame;
                tokio::select! {
                    _ = session_changed(&mut viewed_changes) => {},
                    _ = session_changed(&mut live_changes) => {},
                    _ = wake.notified() => {},
                    _ = Timer::after(maintenance) => continue,
                }
                // Coalesce a burst without polling the UI on every output batch.
                Timer::after(next_frame.saturating_duration_since(Instant::now())).await;
            }
        });
        let mut view = Self {
            runs: session.into_iter().collect(),
            viewed_run: 0,
            generation: 0,
            settings: settings.clone(),
            unread: false,
            observed_revision: 0,
            observed_view_revision: 0,
            observed_insert_ready: false,
            observed_directory: None,
            trust_prompt: false,
            zmodem_prompt: None,
            zmodem: None,
            window_state,
            selection_drag: None,
            selection_pointer: None,
            search_input,
            search_open: false,
            search_case: false,
            show_final_alternate: false,
            _search_subscription: search_subscription,
            snapshot,
            snapshot_task: None,
            command_task: None,
            focus,
            menu_focus,
            menu_open: false,
            context_menu_position: None,
            menu_index: 0,
            menu_scroll: ScrollHandle::default(),
            _menu_blur: menu_blur,
            index,
            display_name: shell.clone(),
            shell,
            target: None,
            local_shell: None,
            kind: TerminalKind::Local,
            active_tool: None,
            header_external: false,
            compact_header: true,
            external_header: None,
            manually_disconnected: false,
            previous_directory: None,
            font,
            metrics_font_size: None,
            font_size,
            initial_font_size: font_size,
            cell_width: px(font_size * 0.6),
            line_height: px(font_size * 1.4),
            bounds: None,
            resize: ResizeScheduler::new((80, 24)),
            resize_receipt: None,
            requested_revision: 0,
            state: i18n_key::COMMON_CONNECTING.into(),
            error,
            dismissed_feedback: None,
            visible: true,
            surface_visible: true,
            preedit: String::new(),
            preedit_selection: 0..0,
            paste_pending: false,
            copy_task: None,
            primary_copy_task: None,
            mouse_button: None,
            last_mouse: (0, 0),
            wheel: Default::default(),
            poll_wake,
            _poll: poll,
            _focus: on_focus,
            _blur: on_blur,
            _activation: activation,
        };
        view.watch_snapshot(cx);
        view.watch_command_completions(cx);
        view
    }

    pub fn sftp_client(&self) -> Option<rustterm::session::SftpClient> {
        self.runs.get(self.viewed_run).and_then(Session::sftp)
    }

    pub fn is_running(&self) -> bool {
        self.runs.last().is_some_and(|s| s.state().is_running())
    }

    /// Short, stable status for the workspace's shared status bar.
    pub fn status_text(&self) -> String {
        let mut label = self
            .runs
            .get(self.viewed_run)
            .map(|run| state_label(run.state()))
            .unwrap_or_else(|| tr(&self.state).to_owned());
        if self.error.is_some() {
            label.push_str(tr(i18n_key::TERMINAL_STATUS_ATTENTION_NEEDED));
        }
        if self.show_final_alternate {
            label.push_str(tr(i18n_key::TERMINAL_STATUS_FINAL_ALTERNATE_READ_ONLY));
        } else if self
            .runs
            .get(self.viewed_run)
            .is_some_and(Session::is_read_only)
        {
            label.push_str(tr(i18n_key::TERMINAL_STATUS_READ_ONLY_HISTORY));
        } else if self.snapshot.as_ref().is_some_and(|s| s.alternate_screen) {
            label.push_str(tr(i18n_key::TERMINAL_STATUS_ALTERNATE_SCREEN));
        }
        if let Some(snapshot) = &self.snapshot {
            if snapshot.viewport_evicted {
                label.push_str(tr(i18n_key::TERMINAL_STATUS_VIEW_POSITION_EVICTED));
            } else if snapshot.display_offset > 0 {
                label.push_str(tr(i18n_key::TERMINAL_STATUS_FOLLOWING_PAUSED));
            }
            if snapshot.selection_invalidated {
                label.push_str(tr(i18n_key::TERMINAL_STATUS_SELECTION_CANCELLED));
            }
        }
        label
    }

    pub fn status_color(&self) -> u32 {
        if self.error.is_some()
            || self
                .runs
                .get(self.viewed_run)
                .is_some_and(|run| matches!(run.state(), SessionState::Failed { .. }))
        {
            ui::colors().danger
        } else if self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.viewport_evicted || s.selection_invalidated || s.display_offset > 0)
        {
            ui::colors().warning
        } else if self.show_final_alternate
            || self
                .runs
                .get(self.viewed_run)
                .is_some_and(Session::is_read_only)
            || self.snapshot.as_ref().is_some_and(|s| s.alternate_screen)
        {
            ui::colors().accent
        } else {
            match self.runs.get(self.viewed_run).map(Session::state) {
                Some(SessionState::Connected) => ui::colors().success,
                Some(
                    SessionState::Connecting
                    | SessionState::WaitingHostKey { .. }
                    | SessionState::WaitingAuthentication,
                ) => ui::colors().warning,
                _ => ui::colors().muted,
            }
        }
    }

    /// Output volume deliberately does not affect this text or parent refreshes.
    pub fn status_details(&self) -> String {
        let (columns, rows) = self
            .snapshot
            .as_ref()
            .map_or(self.resize.requested_size(), |s| (s.columns, s.rows));
        let details = rustterm::trf!(
            i18n_key::TERMINAL_COLUMNS_ROWS_UTF_8_LINE_NUMBERS_LINE_NUMBERS_TIMESTAMPS_TIMESTAMPS_HIGHLIGHT_HIGHLIGHT,
            columns = columns,
            rows = rows,
            line_numbers = if self.settings.line_numbers {
                tr(i18n_key::COMMON_ON)
            } else {
                tr(i18n_key::COMMON_OFF)
            },
            timestamps = if self.settings.line_timestamps {
                tr(i18n_key::COMMON_ON)
            } else {
                tr(i18n_key::COMMON_OFF)
            },
            highlight = if self.settings.highlight {
                tr(i18n_key::COMMON_ON)
            } else {
                tr(i18n_key::COMMON_OFF)
            },
        );
        if let Some(command) = self.command_status() {
            format!("{command}  ·  {details}")
        } else {
            details
        }
    }

    pub fn may_auto_reconnect(&self) -> bool {
        if self.manually_disconnected || self.kind == TerminalKind::Local {
            return false;
        }
        self.runs.last().is_some_and(|session| {
            matches!(
                session.state(),
                SessionState::Failed {
                    kind: FailureKind::Dns
                        | FailureKind::Network
                        | FailureKind::Timeout
                        | FailureKind::Read
                        | FailureKind::Write
                        | FailureKind::RemoteClosed,
                    ..
                } | SessionState::Disconnected { exit_code: None }
            )
        })
    }

    pub fn forwarding(&self) -> Option<rustterm::session::ForwardController> {
        self.runs.last().and_then(Session::forwarding)
    }

    pub fn disconnect_or_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            self.manually_disconnected = true;
            if let Some(session) = self.runs.last() {
                session.disconnect();
            }
        } else {
            cx.emit(PaneEvent::Reconnect(self.index));
        }
        cx.notify();
    }

    pub fn diagnostic_state(&self) -> &'static str {
        self.runs
            .last()
            .map(|s| diagnostic_label(s.state()))
            .unwrap_or("not_started")
    }

    fn prune_evicted(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.runs.iter().any(Session::is_evicted) {
            return;
        }
        let viewed_id = self.runs.get(self.viewed_run).map(Session::id);
        self.runs.retain(|run| !run.is_evicted());
        if let Some(index) =
            viewed_id.and_then(|id| self.runs.iter().position(|run| run.id() == id))
        {
            // Removing another archive only changes this session's index. Its
            // search, input capture and in-flight resize still belong to it.
            self.viewed_run = index;
        } else {
            // The evicted session is already stopped. Discard its capture before
            // clearing local input, so no release can reach a retained session.
            self.mouse_button = None;
            self.release_input();
            self.viewed_run = self.runs.len().saturating_sub(1);
            self.generation += 1;
            self.trust_prompt = false;
            self.show_final_alternate = false;
            self.reset_search(cx);
            self.snapshot = self.runs.get(self.viewed_run).map(Session::snapshot);
            self.requested_revision = 0;
            self.observed_view_revision = 0;
            self.resize = ResizeScheduler::new((0, 0));
            self.resize_receipt = None;
            self.watch_snapshot(cx);
            self.error = Some(
                i18n_key::TERMINAL_HISTORY_FROM_AN_EARLIER_CONNECTION_WAS_EVICTED_BY_THE_HISTORY
                    .into(),
            );
            if self.runs.is_empty() {
                self.state = i18n_key::TERMINAL_HISTORY_EVICTED.into();
            } else if self.visible && window.is_window_active() && self.focus.is_focused(window) {
                self.report_focus(true);
            }
        }
        for (index, run) in self.runs.iter().enumerate() {
            run.set_visible(
                self.visible
                    && self.window_state.visibility.is_visible()
                    && !self.window_state.is_locked()
                    && index == self.viewed_run,
            );
        }
        self.apply_history_budget();
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    pub(super) fn toggle_final_alternate(&mut self, cx: &mut Context<Self>) {
        self.poll_wake.notify_one();
        self.copy_task.take();
        self.primary_copy_task.take();
        self.reset_search(cx);
        self.selection_drag = None;
        self.selection_pointer = None;
        if self.show_final_alternate {
            self.show_final_alternate = false;
            self.snapshot = self.runs.get(self.viewed_run).map(Session::snapshot);
            self.requested_revision = 0;
            self.observed_view_revision = 0;
            self.resize = ResizeScheduler::new((0, 0));
            self.resize_receipt = None;
        } else if let Some(snapshot) = self
            .runs
            .get(self.viewed_run)
            .and_then(Session::final_alt_snapshot)
        {
            self.show_final_alternate = true;
            self.snapshot = Some(snapshot);
        }
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    fn report_focus(&self, focused: bool) {
        if self.viewed_run + 1 == self.runs.len()
            && !self.show_final_alternate
            && let Some(run) = self.runs.get(self.viewed_run)
            && !run.is_read_only()
            && run.state() == SessionState::Connected
        {
            // Lifecycle notifications must not replace feedback from a user
            // operation. The actor applies the terminal's focus-reporting mode.
            let _ = run.send(Command::Focus(focused));
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.release_input();
        if !visible {
            self.report_focus(false);
        }
        self.visible = visible;
        self.poll_wake.notify_one();
        if !visible {
            self.menu_open = false;
            self.context_menu_position = None;
        }
        self.surface_visible =
            visible && self.window_state.visibility.is_visible() && !self.window_state.is_locked();
        for (i, run) in self.runs.iter().enumerate() {
            run.set_visible(self.surface_visible && i == self.viewed_run);
        }
        if visible {
            self.unread = false;
            self.requested_revision = 0;
            self.observed_view_revision = 0;
        }
        cx.notify();
    }

    fn apply_history_budget(&self) {
        let count = self.runs.len().max(1);
        for run in &self.runs {
            let _ = run.send(Command::SetHistoryBudget {
                lines: self.settings.history_lines / count,
                bytes: self.settings.history_bytes / count,
            });
        }
    }

    pub fn apply_settings(&mut self, settings: &Settings, cx: &mut Context<Self>) {
        self.settings = settings.clone();
        self.search_input.update(cx, |_, cx| cx.notify());
        self.font = terminal_font(&settings.font_family);
        self.metrics_font_size = None;
        self.font_size = settings.font_size;
        self.initial_font_size = settings.font_size;
        for run in &self.runs {
            let _ = run.send(Command::SetHighlight(settings.highlight));
            let _ = run.send(Command::SetCommandOutputCache(
                settings.cache_command_output,
            ));
            let _ = run.send(Command::SetTheme(settings.theme.clone()));
        }
        self.apply_history_budget();
        self.requested_revision = 0;
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    pub fn reconnect(&mut self, session: Session, cx: &mut Context<Self>) {
        self.poll_wake.notify_one();
        self.manually_disconnected = false;
        self.reset_search(cx);
        self.release_input();
        self.show_final_alternate = false;
        if let Some(old) = self.runs.last() {
            old.disconnect();
        }
        for run in &self.runs {
            run.set_visible(false);
        }
        self.generation += 1;
        if self.runs.len() >= 8 {
            self.runs.remove(0).close();
        }
        session.set_visible(
            self.visible
                && self.window_state.visibility.is_visible()
                && !self.window_state.is_locked(),
        );
        let _ = session.send(Command::SetHighlight(self.settings.highlight));
        let _ = session.send(Command::SetCommandOutputCache(
            self.settings.cache_command_output,
        ));
        let _ = session.send(Command::SetTheme(self.settings.theme.clone()));
        self.runs.push(session);
        self.viewed_run = self.runs.len() - 1;
        self.snapshot = Some(self.runs[self.viewed_run].snapshot());
        self.requested_revision = 0;
        self.observed_view_revision = 0;
        self.resize = ResizeScheduler::new((0, 0));
        self.resize_receipt = None;
        self.error = None;
        self.search_open = false;
        self.trust_prompt = false;
        self.apply_history_budget();
        self.watch_snapshot(cx);
        self.watch_command_completions(cx);
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    pub(super) fn choose_run(&mut self, run: usize, window: &Window, cx: &mut Context<Self>) {
        self.poll_wake.notify_one();
        self.reset_search(cx);
        self.release_input();
        self.report_focus(false);
        self.show_final_alternate = false;
        self.generation += 1;
        self.trust_prompt = false;
        self.viewed_run = run.min(self.runs.len().saturating_sub(1));
        self.search_open = false;
        for (i, session) in self.runs.iter().enumerate() {
            session.set_visible(
                self.visible
                    && self.window_state.visibility.is_visible()
                    && !self.window_state.is_locked()
                    && i == self.viewed_run,
            );
        }
        self.snapshot = self.runs.get(self.viewed_run).map(Session::snapshot);
        self.requested_revision = 0;
        self.observed_view_revision = 0;
        self.resize = ResizeScheduler::new((0, 0));
        self.resize_receipt = None;
        self.watch_snapshot(cx);
        if self.visible && window.is_window_active() && self.focus.is_focused(window) {
            self.report_focus(true);
        }
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.copy_task.take();
        self.primary_copy_task.take();
        self.selection_drag = None;
        self.selection_pointer = None;
        self.snapshot_task.take();
        self.command_task.take();
        self.preedit.clear();
        for session in &self.runs {
            session.close();
        }
        cx.notify();
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            self.copy_task.take();
            self.primary_copy_task.take();
            self.menu_open = false;
            self.context_menu_position = None;
            self.selection_drag = None;
            self.selection_pointer = None;
        } else if self.menu_open && !window.is_window_active() {
            self.close_menu(window, cx);
        }
        let surface_visible = self.visible
            && self.window_state.visibility.is_visible()
            && !self.window_state.is_locked();
        if surface_visible != self.surface_visible {
            self.surface_visible = surface_visible;
            for (index, run) in self.runs.iter().enumerate() {
                run.set_visible(surface_visible && index == self.viewed_run);
            }
            if surface_visible {
                self.unread = false;
                self.requested_revision = 0;
                self.observed_view_revision = 0;
                cx.notify();
            }
        }
        self.poll_transfer(window, cx);
        let Some(session) = self.runs.get(self.viewed_run) else {
            return;
        };
        let session_state = session.state();
        let previous_status = self.status_text();
        let live_revision = self.runs.last().map(Session::revision).unwrap_or(0);
        if !surface_visible && live_revision != self.observed_revision && !self.unread {
            self.unread = true;
            cx.emit(PaneEvent::Activity);
        }
        self.observed_revision = live_revision;
        let state = state_label(session_state.clone());
        let state_changed = state != self.state;
        let insert_ready = self.can_insert_history_command(session.id());
        let directory = session.current_directory().map(|path| (session.id(), path));
        // Keep the run identity: reconnecting to the same path must still
        // synchronize the replacement SFTP channel, without requesting a grid.
        // Locked workspaces reject pane events, so defer observing until unlock.
        let directory_changed =
            !self.window_state.is_locked() && directory != self.observed_directory;
        if directory_changed {
            self.observed_directory = directory;
            if self.viewed_run + 1 == self.runs.len()
                && let Some((_, path)) = &self.observed_directory
            {
                if self.previous_directory.as_ref() == Some(path) {
                    self.previous_directory = None;
                }
                cx.emit(PaneEvent::DirectoryChanged(self.index, path.clone()));
            }
        }
        let mut changed =
            state_changed || insert_ready != self.observed_insert_ready || directory_changed;
        self.observed_insert_ready = insert_ready;
        self.state = state;
        if surface_visible && !self.show_final_alternate {
            let revision = session.revision();
            // Ask GPUI for a frame; only actual canvas prepaint requests grid data.
            // A minimized window can keep receiving output without generating snapshots.
            if revision != self.observed_view_revision {
                self.observed_view_revision = revision;
                changed = true;
            }
            let snapshot = session.snapshot();
            changed |= self.apply_snapshot(snapshot, cx);
        }
        // No notify on each output batch, and no snapshot requests for hidden panes.
        if changed {
            cx.notify();
        }
        if state_changed || previous_status != self.status_text() {
            cx.emit(PaneEvent::Activity);
        }
        if let SessionState::WaitingHostKey {
            host,
            port,
            algorithm,
            fingerprint,
        } = session_state
            && !self.trust_prompt
            && surface_visible
            && window.is_window_active()
            && self.focus.is_focused(window)
            && !self.composing()
            && let Some(permit) = self.window_state.prompts.acquire()
        {
            self.trust_prompt = true;
            let session_id = self.runs[self.viewed_run].id();
            let response=window.prompt(PromptLevel::Warning,tr(i18n_key::TERMINAL_FIRST_CONNECTION_VERIFY_THE_HOST_KEY),Some(&rustterm::trf!(i18n_key::TERMINAL_HOST_HOST_PORT_ALGORITHM_ALGORITHM_SHA_256_FINGERPRINT_FINGERPRINT_VERIFY, host = host, port = port, algorithm = algorithm, fingerprint = fingerprint)),&[tr(i18n_key::COMMON_CANCEL),tr(i18n_key::TERMINAL_TRUST_AND_SAVE)],cx);
            cx.spawn_in(window, async move |this, cx| {
                let trusted = response.await == Ok(1);
                let _ = this.update(cx, |this, cx| {
                    let trusted = trusted && permit.is_valid();
                    drop(permit);
                    if let Some(session) = this.runs.iter().find(|s| s.id() == session_id)
                        && matches!(session.state(), SessionState::WaitingHostKey { .. })
                    {
                        let _ = session.send(Command::TrustHostKey(trusted));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        let direction = self.selection_scroll_direction();
        if direction != 0
            && let Some(pointer) = self.selection_pointer
        {
            if self.snapshot.as_ref().is_some_and(|snapshot| {
                !snapshot.alternate_screen
                    && if direction > 0 {
                        snapshot.display_offset < snapshot.history_size
                    } else {
                        snapshot.display_offset > 0
                    }
            }) && let Some(drag) = &mut self.selection_drag
            {
                drag.scrolled();
            }
            self.send(Command::Scroll(direction), cx);
            self.extend_selection(pointer, cx);
        }
        self.flush_resize();
    }

    fn selection_scroll_direction(&self) -> i32 {
        if self.window_state.is_locked() || self.selection_drag.is_none() {
            return 0;
        }
        match (self.selection_pointer, self.bounds) {
            (Some(pointer), Some(bounds)) if pointer.y < bounds.top() => 3,
            (Some(pointer), Some(bounds)) if pointer.y > bounds.bottom() => -3,
            _ => 0,
        }
    }

    pub(super) fn flush_resize(&mut self) {
        if self
            .resize_receipt
            .as_ref()
            .is_some_and(ResizeReceipt::is_complete)
        {
            self.resize_receipt = None;
            self.resize.complete();
        }
        if !self.visible || !self.window_state.visibility.is_visible() || self.show_final_alternate
        {
            return;
        }
        let now = Instant::now();
        let Some(session) = self.runs.get(self.viewed_run) else {
            return;
        };
        if let Some((columns, rows)) = self.resize.next(now)
            && let Ok(receipt) = session.resize(columns, rows)
        {
            self.resize.sent((columns, rows), now);
            self.resize_receipt = Some(receipt);
        }
        if self.resize.is_pending() {
            // Layout can change inside the resize cooldown, before there is an
            // actor command to publish a revision. Keep its final size timely.
            self.poll_wake.notify_one();
        }
    }

    pub(super) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            return;
        }
        if self.show_final_alternate
            && matches!(
                command,
                Command::Input(_)
                    | Command::Key { .. }
                    | Command::Paste(_)
                    | Command::AlternateScroll(_)
                    | Command::InsertCommandDraft { .. }
                    | Command::Mouse { .. }
                    | Command::BeginSelection { .. }
                    | Command::SelectAll
                    | Command::UpdateSelection { .. }
                    | Command::Search { .. }
                    | Command::NextMatch { .. }
                    | Command::Scroll(_)
                    | Command::ScrollTo(_)
                    | Command::ScrollToBottom
                    | Command::NavigateCommand { .. }
                    | Command::SelectCommandOutput { .. }
                    | Command::RevealCommand { .. }
            )
        {
            return;
        }
        if self.viewed_run + 1 < self.runs.len()
            && matches!(
                command,
                Command::Input(_)
                    | Command::Key { .. }
                    | Command::Paste(_)
                    | Command::AlternateScroll(_)
                    | Command::InsertCommandDraft { .. }
                    | Command::Mouse { .. }
            )
        {
            return;
        }
        if let Some(session) = self.runs.get(self.viewed_run) {
            if let Err(error) = session.send(command) {
                self.error = Some(rustterm::i18n::Message::with_arg(
                    i18n_key::TERMINAL_INPUT_WAS_NOT_ACCEPTED_ERROR,
                    "error",
                    rustterm::i18n::Message::from_error(error),
                ));
                cx.emit(PaneEvent::Activity);
                cx.notify();
            } else if self.error.take().is_some() {
                cx.emit(PaneEvent::Activity);
                cx.notify();
            }
        }
    }
}

fn diagnostic_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Connecting => "connecting",
        SessionState::WaitingHostKey { .. } => "waiting_host_key",
        SessionState::WaitingAuthentication => "waiting_authentication",
        SessionState::Connected => "connected",
        SessionState::Closing => "closing",
        SessionState::Disconnected { .. } => "disconnected",
        SessionState::Failed { kind, .. } => match kind {
            FailureKind::Spawn => "failed_spawn",
            FailureKind::Read => "failed_read",
            FailureKind::Write => "failed_write",
            FailureKind::Resize => "failed_resize",
            FailureKind::Process => "failed_process",
            FailureKind::Backpressure => "failed_backpressure",
            FailureKind::Dns => "failed_dns",
            FailureKind::Network => "failed_network",
            FailureKind::Timeout => "failed_timeout",
            FailureKind::Authentication => "failed_authentication",
            FailureKind::HostKeyMismatch => "failed_host_key_mismatch",
            FailureKind::HostKeyRejected => "failed_host_key_rejected",
            FailureKind::KnownHosts => "failed_known_hosts",
            FailureKind::PrivateKey => "failed_private_key",
            FailureKind::RemoteClosed => "failed_remote_closed",
            FailureKind::Protocol => "failed_protocol",
        },
    }
}

fn failure_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Spawn => tr(i18n_key::TERMINAL_PROCESS_START),
        FailureKind::Read => tr(i18n_key::TERMINAL_TERMINAL_READ),
        FailureKind::Write => tr(i18n_key::TERMINAL_TERMINAL_WRITE),
        FailureKind::Resize => tr(i18n_key::TERMINAL_TERMINAL_RESIZE),
        FailureKind::Process => tr(i18n_key::TERMINAL_PROCESS),
        FailureKind::Backpressure => tr(i18n_key::TERMINAL_TERMINAL_INPUT_QUEUE),
        FailureKind::Dns => tr(i18n_key::TERMINAL_NAME_RESOLUTION),
        FailureKind::Network => tr(i18n_key::TERMINAL_NETWORK),
        FailureKind::Timeout => tr(i18n_key::TERMINAL_TIMEOUT),
        FailureKind::Authentication => tr(i18n_key::COMMON_AUTHENTICATION),
        FailureKind::HostKeyMismatch => tr(i18n_key::TERMINAL_HOST_KEY_CHANGED),
        FailureKind::HostKeyRejected => tr(i18n_key::TERMINAL_HOST_KEY_REJECTED),
        FailureKind::KnownHosts => tr(i18n_key::TERMINAL_KNOWN_HOSTS),
        FailureKind::PrivateKey => tr(i18n_key::COMMON_PRIVATE_KEY),
        FailureKind::RemoteClosed => tr(i18n_key::TERMINAL_REMOTE_CONNECTION_CLOSED),
        FailureKind::Protocol => tr(i18n_key::TERMINAL_PROTOCOL),
    }
}

pub(super) fn state_label(state: SessionState) -> String {
    match state {
        SessionState::Connecting => tr(i18n_key::COMMON_CONNECTING).into(),
        SessionState::WaitingHostKey { .. } => {
            tr(i18n_key::TERMINAL_WAITING_FOR_HOST_CONFIRMATION).into()
        }
        SessionState::WaitingAuthentication => {
            tr(i18n_key::TERMINAL_WAITING_FOR_AUTHENTICATION).into()
        }
        SessionState::Connected => tr(i18n_key::COMMON_CONNECTED).into(),
        SessionState::Closing => tr(i18n_key::TERMINAL_CLOSING).into(),
        SessionState::Disconnected { exit_code } => match exit_code {
            Some(code) => rustterm::trf!(i18n_key::TERMINAL_EXITED_CODE, code = code),
            None => tr(i18n_key::TERMINAL_EXITED).into(),
        },
        SessionState::Failed { kind, message } => rustterm::trf!(
            i18n_key::TERMINAL_FAILED_KIND_MESSAGE,
            kind = failure_label(kind),
            message = message
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_panes_keep_only_the_maintenance_timer() {
        for (visible, focused, frame) in [(true, true, 16), (true, false, 34), (false, true, 250)] {
            assert_eq!(
                poll_intervals(visible, focused, false),
                (Duration::from_millis(frame), Duration::from_millis(250)),
            );
        }
    }

    #[test]
    fn selection_and_resize_keep_the_frame_interval_only_while_visible() {
        for (visible, focused, period) in [(true, true, 16), (true, false, 34), (false, true, 250)]
        {
            assert_eq!(
                poll_intervals(visible, focused, true),
                (Duration::from_millis(period), Duration::from_millis(period)),
            );
        }
    }
}
