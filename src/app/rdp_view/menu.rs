use super::*;
use gpui::{AnyElement, CursorStyle, KeyDownEvent, MouseButton, div, px, rgb};
use rustterm::i18n::messages as i18n_key;
use ui::Icon;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Actions,
    ConnectionInfo,
}

#[derive(Clone, Copy)]
enum Action {
    Actual,
    SendClipboard,
    SendFiles,
    SendFolders,
    CancelFiles,
    CopyRemote,
    RequestClipboard,
    MuteAudio,
    Microphone,
    SecureAttention,
    ConnectionInfo,
    KeyboardHelp,
    Disconnect,
}

const ACTIONS: [Action; 13] = [
    Action::Actual,
    Action::SendClipboard,
    Action::SendFiles,
    Action::SendFolders,
    Action::CancelFiles,
    Action::CopyRemote,
    Action::RequestClipboard,
    Action::MuteAudio,
    Action::Microphone,
    Action::SecureAttention,
    Action::ConnectionInfo,
    Action::KeyboardHelp,
    Action::Disconnect,
];

impl RdpView {
    pub(super) fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.surface_visible()
            || !window.is_window_active()
            || self.closed
            || !self.preedit.is_empty()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if self.menu_open {
            self.close_menu(window, cx);
        } else {
            self.release_control(cx);
            self.menu_open = true;
            self.menu_page = Page::Actions;
            self.menu_index = ACTIONS
                .iter()
                .position(|action| self.menu_action_enabled(*action))
                .unwrap();
            self.menu_scroll.scroll_to_item(self.menu_index);
            cx.emit(PaneEvent::Focused(self.index));
            self.menu_focus.focus(window);
            cx.notify();
        }
    }

    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.menu_page = Page::Actions;
        self.focus.focus(window);
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    fn menu_action_enabled(&self, action: Action) -> bool {
        match action {
            Action::SendFiles | Action::SendFolders => {
                self.state == State::Connected
                    && self.config.clipboard
                    && self.config.clipboard_files
            }
            Action::CancelFiles => {
                self.state == State::Connected
                    && (self.clipboard.has_file_offer()
                        || self
                            .incoming_files
                            .as_ref()
                            .is_some_and(|transfer| !transfer.ready)
                        || self
                            .file_transfer
                            .as_ref()
                            .is_some_and(|transfer| transfer.available))
            }
            Action::MuteAudio => self.state == State::Connected && self.config.audio_output,
            Action::Microphone => self.state == State::Connected && self.config.audio_input,
            Action::SecureAttention | Action::ConnectionInfo => self.state == State::Connected,
            Action::SendClipboard | Action::RequestClipboard | Action::CopyRemote => {
                self.state == State::Connected
                    && self.config.clipboard
                    && (!matches!(action, Action::CopyRemote)
                        || self.clipboard.remote.is_some()
                        || self.clipboard.remote_content.is_some())
            }
            Action::Actual | Action::KeyboardHelp | Action::Disconnect => true,
        }
    }

    fn run_menu_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if !self.menu_open
            || !self.surface_visible()
            || !window.is_window_active()
            || self.closed
            || self.window_state.prompts.is_busy()
            || !self.menu_action_enabled(action)
        {
            return;
        }
        if matches!(action, Action::ConnectionInfo) {
            self.menu_page = Page::ConnectionInfo;
            self.menu_scroll.set_offset(gpui::point(px(0.), px(0.)));
            cx.notify();
            return;
        }
        self.close_menu(window, cx);
        match action {
            Action::Actual => self.zoom = Zoom::Actual,
            Action::SendClipboard => self.send_clipboard(window, cx),
            Action::SendFiles => self.choose_files(false, cx),
            Action::SendFolders => self.choose_files(true, cx),
            Action::CancelFiles => self.cancel_file_transfer(cx),
            Action::CopyRemote => self.copy_clipboard(cx),
            Action::RequestClipboard => self.request_clipboard(cx),
            Action::MuteAudio => self.toggle_audio_mute(cx),
            Action::Microphone => {
                let enabled = !self.microphone_enabled;
                if self.send(RdpCommand::SetMicrophoneEnabled(enabled), cx) {
                    self.microphone_enabled = enabled;
                }
            }
            Action::SecureAttention => {
                // The complete chord leaves no modifiers held. The mailbox
                // guarantees ReleaseAll if any part encounters backpressure.
                for (scancode, pressed) in [
                    (0x1d, true),
                    (0x38, true),
                    (0xe053, true),
                    (0xe053, false),
                    (0x38, false),
                    (0x1d, false),
                ] {
                    if !self.send(RdpCommand::Key { scancode, pressed }, cx) {
                        break;
                    }
                }
            }
            Action::ConnectionInfo => unreachable!("connection information keeps the menu open"),
            Action::KeyboardHelp => {
                self.notice = Some(i18n_key::RDP_CONTROL_CLIPBOARD_HELP);
            }
            Action::Disconnect => self.disconnect_or_reconnect(cx),
        }
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    pub(super) fn menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.menu_open {
            return false;
        }
        if self.menu_page == Page::ConnectionInfo {
            if matches!(
                event.keystroke.key.as_str(),
                "escape" | "left" | "backspace" | "enter" | "return" | "space"
            ) {
                self.menu_page = Page::Actions;
                self.menu_scroll.scroll_to_item(self.menu_index);
                cx.notify();
            }
            window.prevent_default();
            cx.stop_propagation();
            return true;
        }
        // Display actions remain available in every connection state.
        let enabled: Vec<_> = ACTIONS
            .iter()
            .enumerate()
            .filter_map(|(index, action)| self.menu_action_enabled(*action).then_some(index))
            .collect();
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "up" | "tab" if event.keystroke.key == "up" || event.keystroke.modifiers.shift => {
                self.menu_index = enabled
                    .iter()
                    .rev()
                    .copied()
                    .find(|index| *index < self.menu_index)
                    .unwrap_or(enabled[enabled.len() - 1]);
            }
            "down" | "tab" => {
                self.menu_index = enabled
                    .iter()
                    .copied()
                    .find(|index| *index > self.menu_index)
                    .unwrap_or(enabled[0]);
            }
            "home" => self.menu_index = enabled[0],
            "end" => self.menu_index = enabled[enabled.len() - 1],
            "enter" | "return" | "space" => {
                self.run_menu_action(ACTIONS[self.menu_index], window, cx);
            }
            _ => {}
        }
        self.menu_scroll.scroll_to_item(self.menu_index);
        // Consume every key while the menu owns focus, including unhandled
        // shortcuts, so none can edit the remote desktop behind it.
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.menu_open {
            return None;
        }
        let mut popup = div()
            .id(("rdp-actions-popup", self.index))
            .absolute()
            .top(px(ui::PANE_HEADER_HEIGHT))
            .right(px(4.))
            .w(px((self.layout_size.0 - 8.).clamp(0., 300.)))
            .max_h(px(
                (self.layout_size.1 - ui::PANE_HEADER_HEIGHT - 8.).max(28.)
            ))
            .py(px(4.))
            .bg(rgb(ui::colors().raised))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded(px(3.))
            .shadow_md()
            .cursor(CursorStyle::Arrow)
            .occlude()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .track_focus(&self.menu_focus)
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            popup = popup
                .on_mouse_down(button, |_, _, cx| cx.stop_propagation())
                .on_mouse_up(button, |_, _, cx| cx.stop_propagation());
        }
        if self.menu_page == Page::Actions {
            for (index, action) in ACTIONS.into_iter().enumerate() {
                let (label, icon) = match action {
                    Action::Actual => (
                        i18n_key::RDP_SCALE_100_PERCENT,
                        if self.zoom == Zoom::Actual {
                            Icon::Check
                        } else {
                            Icon::Desktop
                        },
                    ),
                    Action::SendClipboard => (i18n_key::RDP_SEND_CLIPBOARD, Icon::ChevronUp),
                    Action::SendFiles => (i18n_key::RDP_SEND_FILES, Icon::File),
                    Action::SendFolders => (i18n_key::RDP_SEND_FOLDERS, Icon::Folder),
                    Action::CancelFiles => (i18n_key::RDP_CANCEL_FILE_TRANSFER, Icon::Close),
                    Action::CopyRemote => (i18n_key::RDP_COPY_FROM_REMOTE, Icon::ChevronDown),
                    Action::RequestClipboard => {
                        (i18n_key::RDP_REQUEST_REMOTE_CLIPBOARD, Icon::Refresh)
                    }
                    Action::MuteAudio => (i18n_key::RDP_MUTE_REMOTE_AUDIO, Icon::Check),
                    Action::Microphone => (
                        if self.microphone_enabled {
                            i18n_key::RDP_PAUSE_MICROPHONE
                        } else {
                            i18n_key::RDP_RESUME_MICROPHONE
                        },
                        Icon::Key,
                    ),
                    Action::SecureAttention => (i18n_key::RDP_SEND_CTRL_ALT_DEL, Icon::Key),
                    Action::ConnectionInfo => (i18n_key::RDP_CONNECTION_INFORMATION, Icon::Desktop),
                    Action::KeyboardHelp => (i18n_key::RDP_RDP_KEYBOARD_SHORTCUTS, Icon::Key),
                    Action::Disconnect if self.is_running() => {
                        (i18n_key::COMMON_DISCONNECT, Icon::Disconnect)
                    }
                    Action::Disconnect => (i18n_key::COMMON_RECONNECT, Icon::Refresh),
                };
                let enabled = self.menu_action_enabled(action);
                let hint = match action {
                    Action::Actual => {
                        i18n_key::RDP_RELEASE_CONTROL_FIRST_CTRL_SHIFT_WHEEL_PANS_AT_100_ADD
                    }
                    _ => label,
                };
                popup = popup.child(
                    div()
                        .id(("rdp-menu-action", index))
                        .h(px(30.))
                        .flex_shrink_0()
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .text_color(rgb(if enabled {
                            ui::colors().text
                        } else {
                            ui::colors().faint
                        }))
                        .when(
                            matches!(action, Action::SendClipboard | Action::KeyboardHelp),
                            |item| item.border_t_1().border_color(rgb(ui::colors().border)),
                        )
                        .when(index == self.menu_index, |item| {
                            item.bg(rgb(ui::colors().selected))
                        })
                        .when(enabled, |item| {
                            item.cursor_pointer().on_mouse_move(cx.listener(
                                move |this, _, _, cx| {
                                    if this.menu_index != index {
                                        this.menu_index = index;
                                        cx.notify();
                                    }
                                },
                            ))
                        })
                        .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(hint).to_owned())).into())
                        .child(
                            ui::icon(icon)
                                .when(
                                    matches!(action, Action::MuteAudio) && !self.audio_muted,
                                    |icon| icon.opacity(0.),
                                )
                                .text_color(rgb(if enabled {
                                    ui::colors().muted
                                } else {
                                    ui::colors().faint
                                })),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(tr(label)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_menu_action(action, window, cx);
                        })),
                );
            }
        } else {
            popup = popup.child(self.connection_information(cx));
        }
        let mut backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .cursor(CursorStyle::Arrow)
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            backdrop = backdrop
                .on_mouse_down(
                    button,
                    cx.listener(|this, _, window, cx| {
                        this.close_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(button, |_, _, cx| cx.stop_propagation());
        }
        Some(
            backdrop
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }

    fn connection_information(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let depth = self.negotiated_color_depth.map_or_else(
            || "—".to_owned(),
            |depth| rustterm::trf!(i18n_key::RDP_DEPTH_BIT, depth = depth),
        );
        let rows = [
            (
                i18n_key::RDP_REMOTE_RESOLUTION,
                self.negotiated_color_depth.map_or_else(
                    || "—".to_owned(),
                    |_| format!("{} × {}", self.desktop_size.0, self.desktop_size.1),
                ),
            ),
            (i18n_key::RDP_NEGOTIATED_COLOR_DEPTH, depth),
            (
                i18n_key::RDP_REQUESTED_DESKTOP_SCALE,
                format!("{}%", self.desktop_scale),
            ),
            (
                i18n_key::RDP_LOCAL_REFRESH_LIMIT,
                rustterm::trf!(i18n_key::RDP_FPS_FPS_MAXIMUM, fps = self.config.max_fps),
            ),
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(10.))
            .child(
                div()
                    .id(("rdp-info-back", self.index))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(ui::colors().text))
                    .child(ui::icon(Icon::ChevronLeft))
                    .child(tr(i18n_key::RDP_CONNECTION_INFORMATION))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu_page = Page::Actions;
                        this.menu_scroll.scroll_to_item(this.menu_index);
                        cx.notify();
                    })),
            )
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .text_size(px(11.))
                    .child(div().text_color(rgb(ui::colors().muted)).child(tr(label)))
                    .child(div().text_color(rgb(ui::colors().text)).child(value))
            }))
    }
}
