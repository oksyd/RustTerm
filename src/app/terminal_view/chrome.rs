use super::lifecycle::state_label;
use super::{TerminalKind, TerminalView};
use crate::app::{
    layout::Axis,
    pane::{PaneEvent, SessionTool},
    ui::{self, Icon},
};
use gpui::{
    AnyElement, App, Context, KeyDownEvent, MouseButton, PromptLevel, Window, div, prelude::*, px,
    rgb,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use rustterm::session::{Session, SessionState};

#[derive(Clone, Copy)]
enum PaneMenuAction {
    Split(Axis),
    Zoom,
    Disconnect,
    Close,
    PreviousRun,
    FinalScreen,
    Details,
    PreviousCommand,
    NextCommand,
    PreviousFailedCommand,
    SelectCommandOutput,
    RestoreDirectory,
}

struct PaneMenuItem {
    action: PaneMenuAction,
    label: &'static str,
    shortcut: &'static str,
    icon: Icon,
}

impl TerminalView {
    fn connection_record(&self) -> String {
        let Some(run) = self.runs.get(self.viewed_run) else {
            return String::new();
        };
        let age = |time: std::time::SystemTime| {
            let seconds = time.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            if seconds < 60 {
                rustterm::trf!(i18n_key::COMMON_TIME_SECONDS_AGO, count = seconds)
            } else if seconds < 3600 {
                rustterm::trf!(i18n_key::COMMON_TIME_MINUTES_AGO, count = seconds / 60)
            } else {
                rustterm::trf!(i18n_key::COMMON_TIME_HOURS_AGO, count = seconds / 3600)
            }
        };
        let ended = run
            .ended_at()
            .map(age)
            .unwrap_or_else(|| tr(i18n_key::TERMINAL_IN_PROGRESS).into());
        rustterm::trf!(
            i18n_key::TERMINAL_CONNECTION_ID_STARTED_STARTED_ENDED_ENDED_STATE,
            id = &run.id().to_string()[..8],
            started = age(run.created_at()),
            ended = ended,
            state = state_label(run.state())
        )
    }

    pub(super) fn feedback_text(&self) -> Option<String> {
        let mut notes = Vec::new();
        if let Some(error) = &self.error {
            notes.push(error.render());
        }
        if let Some(snapshot) = &self.snapshot {
            if snapshot.shell.command_insert_blocked {
                notes.push(
                    tr(i18n_key::TERMINAL_INSERT_REQUIRES_A_SINGLE_LINE_COMMAND_AND_AN_IDLE_SHELL)
                        .into(),
                );
            }
            if snapshot.shell.command_unavailable {
                notes.push(tr(i18n_key::TERMINAL_NO_RETAINED_COMMAND_LOCATION_IS_AVAILABLE_CAPTURED_OUTPUT_CAN_STILL).into());
            }
            if snapshot.viewport_evicted {
                notes.push(
                    tr(i18n_key::TERMINAL_THE_VIEW_POSITION_WAS_EVICTED_SHOWING_THE_EARLIEST_RETAINED_CONTENT)
                        .into(),
                );
            }
            if snapshot.selection_invalidated {
                notes.push(
                    tr(i18n_key::TERMINAL_THE_SELECTED_CONTENT_CHANGED_SELECTION_WAS_CANCELLED)
                        .into(),
                );
            }
            if self.show_final_alternate {
                notes.push(tr(i18n_key::TERMINAL_FINAL_ALTERNATE_SCREEN_SNAPSHOT_READ_ONLY).into());
            } else if snapshot.alternate_screen {
                notes
                    .push(tr(i18n_key::TERMINAL_ALTERNATE_SCREEN_MAIN_BUFFER_SEARCH_PAUSED).into());
            } else if snapshot.display_offset > 0 {
                notes.push(
                    tr(i18n_key::TERMINAL_BROWSING_HISTORY_OUTPUT_IS_STILL_BEING_RECEIVED).into(),
                );
            }
        }
        if self
            .runs
            .get(self.viewed_run)
            .is_some_and(Session::is_read_only)
        {
            notes.push(self.connection_record());
        }
        (!notes.is_empty()).then(|| notes.join(" · "))
    }

    pub(super) fn chrome_blocked(&self, cx: &App) -> bool {
        self.input_composing(cx)
            || self.paste_pending
            || self.window_state.prompts.is_busy()
            || self.window_state.prompts.is_blocked()
    }

    pub(super) fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chrome_blocked(cx) {
            return;
        }
        if self.menu_open && self.context_menu_position.is_none() {
            self.close_menu(window, cx);
        } else {
            self.menu_open = true;
            self.context_menu_position = None;
            self.menu_index = 0;
            self.menu_scroll.scroll_to_item(0);
            cx.emit(PaneEvent::Focused(self.index));
            self.menu_focus.focus(window);
            cx.notify();
        }
    }

    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.context_menu_position = None;
        if !self.window_state.is_locked() {
            self.focus.focus(window);
        }
        cx.notify();
    }

    fn menu_items(&self) -> Vec<PaneMenuItem> {
        use PaneMenuAction::*;
        let mut items = vec![
            PaneMenuItem {
                action: Split(Axis::Horizontal),
                label: i18n_key::TERMINAL_SPLIT_LEFT_RIGHT,
                shortcut: "Ctrl+Shift+D",
                icon: Icon::SplitHorizontal,
            },
            PaneMenuItem {
                action: Split(Axis::Vertical),
                label: i18n_key::TERMINAL_SPLIT_TOP_BOTTOM,
                shortcut: "Ctrl+Shift+E",
                icon: Icon::SplitVertical,
            },
            PaneMenuItem {
                action: Zoom,
                label: i18n_key::WORKSPACE_ZOOM_RESTORE_PANE,
                shortcut: "Ctrl+Shift+Enter",
                icon: Icon::Maximize,
            },
        ];
        for (action, label, shortcut) in [
            (
                PreviousCommand,
                i18n_key::TERMINAL_PREVIOUS_COMMAND,
                "Ctrl+Shift+↑",
            ),
            (NextCommand, i18n_key::TERMINAL_NEXT_COMMAND, "Ctrl+Shift+↓"),
            (
                PreviousFailedCommand,
                i18n_key::TERMINAL_PREVIOUS_FAILED_COMMAND,
                "",
            ),
            (
                SelectCommandOutput,
                i18n_key::TERMINAL_SELECT_COMMAND_OUTPUT,
                "",
            ),
        ] {
            items.push(PaneMenuItem {
                action,
                label,
                shortcut,
                icon: Icon::Terminal,
            });
        }
        if self.previous_directory.is_some() {
            items.push(PaneMenuItem {
                action: RestoreDirectory,
                label: i18n_key::TERMINAL_RESTORE_DIRECTORY_DIALOG,
                shortcut: "",
                icon: Icon::Folder,
            });
        }
        if self.runs.len() > 1 {
            items.push(PaneMenuItem {
                action: PreviousRun,
                label: i18n_key::TERMINAL_VIEW_EARLIER_CONNECTIONS,
                shortcut: "",
                icon: Icon::Folder,
            });
        }
        if self
            .runs
            .get(self.viewed_run)
            .and_then(Session::final_alt_snapshot)
            .is_some()
        {
            items.push(PaneMenuItem {
                action: FinalScreen,
                label: if self.show_final_alternate {
                    i18n_key::TERMINAL_VIEW_MAIN_BUFFER
                } else {
                    i18n_key::TERMINAL_VIEW_FINAL_ALTERNATE_SCREEN
                },
                shortcut: "",
                icon: Icon::Terminal,
            });
        }
        items.push(PaneMenuItem {
            action: Details,
            label: i18n_key::TERMINAL_CONNECTION_AND_STATUS_DETAILS,
            shortcut: "",
            icon: Icon::More,
        });
        items.push(PaneMenuItem {
            action: Disconnect,
            label: if self.is_running() {
                i18n_key::COMMON_DISCONNECT
            } else {
                i18n_key::COMMON_RECONNECT
            },
            shortcut: if self.is_running() {
                ""
            } else {
                "Ctrl+Shift+R"
            },
            icon: if self.is_running() {
                Icon::Disconnect
            } else {
                Icon::Refresh
            },
        });
        items.push(PaneMenuItem {
            action: Close,
            label: i18n_key::TERMINAL_CLOSE_PANE,
            shortcut: "Ctrl+Shift+W",
            icon: Icon::Close,
        });
        items
    }

    fn menu_action_enabled(&self, action: PaneMenuAction) -> bool {
        match action {
            PaneMenuAction::Split(axis) => self.can_split(axis),
            PaneMenuAction::PreviousCommand
            | PaneMenuAction::NextCommand
            | PaneMenuAction::PreviousFailedCommand
            | PaneMenuAction::SelectCommandOutput => {
                !self.show_final_alternate
                    && self.snapshot.as_ref().is_some_and(|snapshot| {
                        !snapshot.alternate_screen && !snapshot.shell.commands.is_empty()
                    })
            }
            PaneMenuAction::RestoreDirectory => {
                self.is_running() && self.viewed_run + 1 == self.runs.len()
            }
            _ => true,
        }
    }

    fn menu_action(&mut self, action: PaneMenuAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.chrome_blocked(cx) || !self.menu_action_enabled(action) {
            return;
        }
        self.close_menu(window, cx);
        cx.emit(PaneEvent::Focused(self.index));
        match action {
            PaneMenuAction::Split(axis) => cx.emit(PaneEvent::Split(axis)),
            PaneMenuAction::Zoom => cx.emit(PaneEvent::Zoom),
            PaneMenuAction::Disconnect => self.disconnect_or_reconnect(cx),
            PaneMenuAction::Close => cx.emit(PaneEvent::Close(self.index)),
            PaneMenuAction::PreviousRun => self.choose_run(
                if self.viewed_run == 0 {
                    self.runs.len() - 1
                } else {
                    self.viewed_run - 1
                },
                window,
                cx,
            ),
            PaneMenuAction::FinalScreen => self.toggle_final_alternate(cx),
            PaneMenuAction::Details => self.show_feedback(window, cx),
            PaneMenuAction::PreviousCommand => self.navigate_command(true, false, cx),
            PaneMenuAction::NextCommand => self.navigate_command(false, false, cx),
            PaneMenuAction::PreviousFailedCommand => self.navigate_command(true, true, cx),
            PaneMenuAction::SelectCommandOutput => self.select_command_output(cx),
            PaneMenuAction::RestoreDirectory => self.restore_previous_directory(window, cx),
        }
    }

    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.menu_open {
            return false;
        }
        if self.context_menu_position.is_some() {
            return self.context_menu_key(event, window, cx);
        }
        let items = self.menu_items();
        self.menu_index = self.menu_index.min(items.len() - 1);
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "up" | "down" | "tab" | "home" | "end" => {
                let enabled: Vec<_> = items
                    .iter()
                    .map(|item| self.menu_action_enabled(item.action))
                    .collect();
                if let Some(index) = ui::popup::next_index(event, self.menu_index, &enabled) {
                    self.menu_index = index;
                }
            }
            "enter" | "return" | "space" => {
                self.menu_action(items[self.menu_index].action, window, cx)
            }
            _ => {}
        }
        self.menu_scroll.scroll_to_item(self.menu_index);
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
        true
    }

    pub(super) fn header_actions(&self, cx: &mut Context<Self>) -> gpui::Div {
        let index = self.index;
        let interactive = !self.chrome_blocked(cx);
        let terminal_visible = self.visible && interactive;
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.))
            .when(!self.is_running(), |d| {
                d.child(
                    ui::icon_button_enabled(
                        ("reconnect", index),
                        Icon::Refresh,
                        if interactive {
                            i18n_key::TERMINAL_RECONNECT_IN_THIS_TAB
                        } else {
                            i18n_key::COMMON_FINISH_THE_CURRENT_INPUT_OR_DIALOG_FIRST
                        },
                        terminal_visible,
                    )
                    .w_auto()
                    .px_2()
                    .gap_1()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().accent))
                    .child(tr(i18n_key::COMMON_RECONNECT))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reconnect_ended(window, cx);
                    })),
                )
            })
            .when(self.viewed_run + 1 < self.runs.len(), |d| {
                d.child(
                    ui::icon_button_enabled(
                        ("live", index),
                        Icon::ChevronRight,
                        i18n_key::TERMINAL_RETURN_TO_LIVE_CONNECTION,
                        terminal_visible,
                    )
                    .when(!self.compact_header, |button| {
                        button
                            .w_auto()
                            .px_2()
                            .gap_1()
                            .text_size(px(11.))
                            .child(tr(i18n_key::TERMINAL_LIVE))
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.visible && !this.chrome_blocked(cx) {
                            this.focus.focus(window);
                            this.choose_run(this.runs.len() - 1, window, cx);
                        }
                    })),
                )
            })
            .when(self.kind == TerminalKind::Ssh, |d| {
                d.child(self.tool_button(SessionTool::Files, cx))
            })
            .when(self.kind != TerminalKind::Container, |d| {
                d.child(self.tool_button(SessionTool::Commands, cx))
            })
            .when(
                matches!(self.kind, TerminalKind::Ssh | TerminalKind::Container),
                |d| d.child(self.tool_button(SessionTool::Resources, cx)),
            )
            .when(self.kind == TerminalKind::Ssh, |d| {
                d.child(self.tool_button(SessionTool::Docker, cx))
                    .child(self.tool_button(SessionTool::Ports, cx))
            })
            .child(
                ui::icon_button_enabled(
                    ("search-open", index),
                    Icon::Search,
                    if !interactive {
                        i18n_key::COMMON_FINISH_THE_CURRENT_INPUT_OR_DIALOG_FIRST
                    } else if !self.visible {
                        i18n_key::COMMON_SHOW_THE_TERMINAL_TO_SEARCH_ITS_OUTPUT
                    } else {
                        i18n_key::TERMINAL_FIND_OUTPUT_CTRL_SHIFT_F
                    },
                    terminal_visible,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.visible && !this.chrome_blocked(cx) {
                        this.open_search(window, cx);
                    }
                })),
            )
            .child(
                ui::icon_button_enabled(
                    ("more", index),
                    Icon::More,
                    if interactive {
                        i18n_key::TERMINAL_PANE_ACTIONS_SHIFT_F10
                    } else {
                        i18n_key::COMMON_FINISH_THE_CURRENT_INPUT_OR_DIALOG_FIRST
                    },
                    terminal_visible,
                )
                .when(self.menu_open, |d| d.bg(rgb(ui::colors().selected)))
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.visible && !this.chrome_blocked(cx) {
                        this.toggle_menu(window, cx);
                    }
                })),
            )
    }

    fn tool_unavailable_reason(&self, tool: SessionTool, cx: &App) -> Option<&'static str> {
        if self.chrome_blocked(cx) {
            return Some(i18n_key::COMMON_FINISH_THE_CURRENT_INPUT_OR_DIALOG_FIRST);
        }
        if tool == SessionTool::Commands {
            return None;
        }
        if self.viewed_run + 1 < self.runs.len() {
            return Some(i18n_key::COMMON_RETURN_TO_THE_LIVE_CONNECTION_TO_USE_THIS_TOOL);
        }
        match self.runs.get(self.viewed_run).map(Session::state) {
            Some(SessionState::Connecting) => {
                return Some(i18n_key::COMMON_AVAILABLE_AFTER_THE_SSH_CONNECTION_IS_ESTABLISHED);
            }
            Some(SessionState::WaitingHostKey { .. }) => {
                return Some(i18n_key::COMMON_CONFIRM_THE_SSH_HOST_BEFORE_USING_THIS_TOOL);
            }
            Some(SessionState::WaitingAuthentication) => {
                return Some(i18n_key::COMMON_FINISH_SSH_AUTHENTICATION_BEFORE_USING_THIS_TOOL);
            }
            Some(SessionState::Connected) => {}
            _ => return Some(i18n_key::COMMON_RECONNECT_SSH_TO_USE_THIS_TOOL),
        }
        match tool {
            SessionTool::Files if self.sftp_client().is_none() => {
                Some(i18n_key::COMMON_SFTP_IS_NOT_AVAILABLE_FOR_THIS_CONNECTION)
            }
            SessionTool::Resources
                if self.kind == TerminalKind::Ssh
                    && self
                        .resource_session()
                        .and_then(Session::resources)
                        .is_none() =>
            {
                Some(i18n_key::COMMON_RESOURCE_MONITORING_IS_NOT_AVAILABLE_FOR_THIS_CONNECTION)
            }
            _ => None,
        }
    }

    fn tool_button(&self, tool: SessionTool, cx: &Context<Self>) -> AnyElement {
        let (id, label, glyph) = match tool {
            SessionTool::Docker => ("session-docker", i18n_key::DOCKER_TITLE, Icon::Container),
            SessionTool::Ports => ("session-ports", i18n_key::PORTS_TITLE, Icon::Link),
            SessionTool::Files => ("session-files", i18n_key::TERMINAL_FILES, Icon::Folder),
            SessionTool::Commands => (
                "session-commands",
                i18n_key::WORKSPACE_COMMANDS,
                Icon::Terminal,
            ),
            SessionTool::Resources => (
                "session-resources",
                i18n_key::RESOURCES_RESOURCES,
                Icon::Pulse,
            ),
        };
        let reason = self.tool_unavailable_reason(tool, cx);
        let enabled = reason.is_none();
        ui::icon_button_enabled((id, self.index), glyph, reason.unwrap_or(label), enabled)
            .when(!self.compact_header, |button| {
                button
                    .w_auto()
                    .px_2()
                    .gap_1()
                    .text_size(px(11.))
                    .child(tr(label))
            })
            .when(self.active_tool == Some(tool), |button| {
                button
                    .bg(rgb(ui::colors().selected))
                    .text_color(rgb(ui::colors().text))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.tool_unavailable_reason(tool, cx).is_some() {
                    return;
                }
                cx.emit(PaneEvent::Focused(this.index));
                cx.emit(match tool {
                    SessionTool::Files => PaneEvent::Files,
                    SessionTool::Commands => PaneEvent::Commands,
                    SessionTool::Resources => PaneEvent::Resources,
                    SessionTool::Docker => PaneEvent::Docker,
                    SessionTool::Ports => PaneEvent::Ports,
                });
            }))
            .into_any_element()
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.menu_open || self.context_menu_position.is_some() {
            return None;
        }
        // The canvas excludes the toolbar and search/transfer rows. The toolbar
        // may also span a docked utility panel, so anchor to its actual bounds.
        let (width, height) = match (self.header_bounds, self.bounds) {
            (Some(header), Some(canvas)) => (
                f32::from(header.size.width),
                f32::from(canvas.bottom() - header.top()).max(ui::PANE_HEADER_HEIGHT),
            ),
            _ => (300., 240.),
        };
        let items = self.menu_items();
        let bounds = ui::popup::fit(
            gpui::point(px(width - 324.), px(ui::PANE_HEADER_HEIGHT)),
            gpui::size(
                px(320.),
                px(items.len() as f32 * ui::metrics::ROW + 2. * ui::popup::PADDING),
            ),
            gpui::Bounds::new(
                gpui::point(px(4.), px(4.)),
                gpui::size(px((width - 8.).max(0.)), px((height - 8.).max(0.))),
            ),
        );
        let mut popup = ui::popup::surface(("pane-menu", self.index))
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .max_h(bounds.size.height)
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .track_focus(&self.menu_focus)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for (index, item) in items.into_iter().enumerate() {
            let action = item.action;
            let enabled = self.menu_action_enabled(action);
            popup =
                popup.child(
                    ui::popup::row(("pane-menu-item", index), enabled, index == self.menu_index)
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if enabled && this.menu_index != index {
                                this.menu_index = index;
                                cx.notify();
                            }
                        }))
                        .child(ui::icon(item.icon))
                        .child(div().flex_1().min_w_0().truncate().child(tr(item.label)))
                        .when(width >= 290., |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(9.))
                                    .text_color(rgb(ui::colors().muted))
                                    .child(item.shortcut),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.menu_action(action, window, cx)
                        })),
                );
        }
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(px(width))
                .h(px(height))
                .occlude()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_any_mouse_down(cx.listener(|this, _, window, cx| {
                    this.close_menu(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }

    pub(super) fn show_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detail = self.feedback_text().unwrap_or_else(|| {
            format!(
                "{}\n{}\n{}",
                self.status_text(),
                self.status_details(),
                self.connection_record()
            )
        });
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Info,
            tr(i18n_key::TERMINAL_TERMINAL_STATUS),
            Some(&detail),
            &[tr(i18n_key::COMMON_CLOSE)],
            cx,
        );
        cx.spawn_in(window, async move |_, _| {
            let _ = answer.await;
            drop(permit);
        })
        .detach();
    }
}

pub(super) fn pane_button(
    action: &'static str,
    pane: usize,
    glyph: Icon,
) -> gpui::Stateful<gpui::Div> {
    let label = match action {
        "more" => i18n_key::WORKSPACE_PANE_ACTIONS,
        "search-open" => i18n_key::TERMINAL_FIND_OUTPUT_CTRL_SHIFT_F,
        "live" => i18n_key::TERMINAL_RETURN_TO_LIVE_CONNECTION,
        "feedback-dismiss" => i18n_key::TERMINAL_DISMISS_NOTICE,
        "bottom" => i18n_key::TERMINAL_SCROLL_TO_BOTTOM_AND_RESUME_FOLLOWING,
        "prev" => i18n_key::TERMINAL_PREVIOUS_MATCH,
        "next" => i18n_key::TERMINAL_NEXT_MATCH,
        "search-close" => i18n_key::TERMINAL_CLOSE_SEARCH_ESCAPE,
        _ => i18n_key::TERMINAL_TERMINAL_ACTION,
    };
    let style = match action {
        "bottom" => ui::ButtonStyle::Primary,
        _ => ui::ButtonStyle::Neutral,
    };
    ui::styled_icon_button((action, pane), glyph, label, style)
        .w_auto()
        .min_w(px(24.))
        .px(px(4.))
        .gap(px(4.))
        .text_size(px(10.))
}
