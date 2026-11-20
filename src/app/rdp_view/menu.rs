use super::*;
use gpui::{AnyElement, CursorStyle, KeyDownEvent, MouseButton, div, px, rgb};
use rustterm::i18n::messages as i18n_key;
use ui::Icon;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Actions,
    ConnectionInfo,
    Display,
}

#[derive(Clone, Copy)]
enum Action {
    Dynamic,
    Fit,
    Actual,
    SendClipboard,
    SendFiles,
    SendFolders,
    CancelFiles,
    CopyRemote,
    MuteAudio,
    Microphone,
    SecureAttention,
    ConnectionInfo,
    KeyboardHelp,
    Disconnect,
}

const DISPLAY_ACTIONS: [Action; 3] = [Action::Dynamic, Action::Fit, Action::Actual];

const ACTIONS: [Action; 11] = [
    Action::SendClipboard,
    Action::SendFiles,
    Action::SendFolders,
    Action::CancelFiles,
    Action::CopyRemote,
    Action::MuteAudio,
    Action::Microphone,
    Action::SecureAttention,
    Action::ConnectionInfo,
    Action::KeyboardHelp,
    Action::Disconnect,
];

impl Page {
    fn actions(self) -> &'static [Action] {
        match self {
            Self::Actions => &ACTIONS,
            Self::Display => &DISPLAY_ACTIONS,
            Self::ConnectionInfo => &[],
        }
    }

    fn rows(self, can_cancel_files: bool) -> impl Iterator<Item = (usize, Action)> {
        // The selection index stays attached to its action when a transfer ends.
        self.actions()
            .iter()
            .copied()
            .enumerate()
            .filter(move |(_, action)| can_cancel_files || !matches!(action, Action::CancelFiles))
    }
}

impl Action {
    fn help(self) -> Option<&'static str> {
        match self {
            Self::Dynamic => Some(i18n_key::RDP_DISPLAY_DYNAMIC_HELP),
            Self::Fit => Some(i18n_key::RDP_SCALE_DESKTOP_TO_FIT_THE_PANE),
            Self::Actual => {
                Some(i18n_key::RDP_RELEASE_CONTROL_FIRST_CTRL_SHIFT_WHEEL_PANS_AT_100_ADD)
            }
            _ => None,
        }
    }
}

impl RdpView {
    pub(super) fn render_display_button(&self, cx: &mut Context<Self>) -> AnyElement {
        ui::text_button(
            ("rdp-display", self.index),
            self.display_label(),
            ui::ButtonStyle::Neutral,
        )
        .tab_index(0)
        .flex_shrink_0()
        .whitespace_nowrap()
        .child(ui::icon(Icon::ChevronDown).size(px(12.)))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(cx.listener(|this, _, window, cx| {
            let display_open = this.menu_open && this.menu_page == Page::Display;
            if this.menu_open {
                this.close_menu(window, cx);
            }
            if !display_open {
                this.toggle_menu(window, cx);
                if this.menu_open {
                    this.select_first_menu_action(Page::Display);
                }
            }
        }))
        .into_any_element()
    }

    pub(super) fn display_label(&self) -> &'static str {
        match self.zoom {
            Zoom::Dynamic => i18n_key::RDP_DISPLAY_AUTO_SHORT,
            Zoom::Fit => i18n_key::RDP_DISPLAY_FIT_SHORT,
            Zoom::Actual => i18n_key::RDP_DISPLAY_ACTUAL_SHORT,
        }
    }

    fn display_details(&self) -> String {
        let scale = self
            .bounds
            .map(|bounds| {
                let desktop = render::desktop_bounds(
                    bounds,
                    self.desktop_size,
                    self.zoom,
                    self.pan,
                    self.device_scale,
                );
                f32::from(desktop.size.width) / f32::from(self.desktop_size.0.max(1))
                    * self.device_scale
                    * 100.
            })
            .unwrap_or(100.);
        let mut text = rustterm::trf!(
            i18n_key::RDP_DISPLAY_CURRENT,
            width = self.desktop_size.0,
            height = self.desktop_size.1,
            scale = format!("{scale:.0}")
        );
        if self.zoom == Zoom::Dynamic
            && self
                .last_resize
                .is_some_and(|size| size != self.desktop_size)
        {
            text.push('\n');
            text.push_str(tr(i18n_key::RDP_DISPLAY_WAITING));
        }
        text
    }

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
            self.select_first_menu_action(Page::Actions);
            cx.emit(PaneEvent::Focused(self.index));
            self.menu_focus.focus(window);
            cx.notify();
        }
    }

    fn menu_rows(&self) -> impl Iterator<Item = (usize, Action)> {
        self.menu_page
            .rows(self.menu_action_enabled(Action::CancelFiles))
    }

    fn select_first_menu_action(&mut self, page: Page) {
        self.menu_page = page;
        self.menu_index = page
            .actions()
            .iter()
            .position(|action| self.menu_action_enabled(*action))
            .unwrap_or_default();
        self.scroll_to_menu_selection();
    }

    fn scroll_to_menu_selection(&self) {
        if let Some(row) = self
            .menu_rows()
            .position(|(index, _)| index == self.menu_index)
        {
            // The display summary is the first scrollable child on that page.
            self.menu_scroll
                .scroll_to_item(row + usize::from(self.menu_page == Page::Display));
        }
    }

    pub(super) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.hide_fullscreen_bar_later(cx);
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
            Action::SendClipboard | Action::CopyRemote => {
                self.state == State::Connected && self.config.clipboard
            }
            Action::Dynamic => self.config.remote_app.is_none() && self.config.monitor_count == 1,
            Action::Fit | Action::Actual | Action::KeyboardHelp | Action::Disconnect => true,
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
            Action::Dynamic | Action::Fit | Action::Actual => {
                self.zoom = match action {
                    Action::Dynamic => Zoom::Dynamic,
                    Action::Fit => Zoom::Fit,
                    _ => Zoom::Actual,
                };
                self.pan = (0., 0.);
                self.restore_pan = None;
                self.display_changed(cx);
                self.last_resize = None;
                self.request_resize(cx);
            }
            Action::SendClipboard => self.send_clipboard(window, cx),
            Action::SendFiles => self.choose_files(false, cx),
            Action::SendFolders => self.choose_files(true, cx),
            Action::CancelFiles => self.cancel_file_transfer(cx),
            Action::CopyRemote => self.copy_clipboard(cx),
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
            Action::ConnectionInfo => return,
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
                self.scroll_to_menu_selection();
                cx.notify();
            }
            window.prevent_default();
            cx.stop_propagation();
            return true;
        }
        let actions = self.menu_page.actions();
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "enter" | "return" | "space" => {
                if let Some(&action) = actions.get(self.menu_index) {
                    self.run_menu_action(action, window, cx);
                }
            }
            _ => {
                let enabled: Vec<_> = actions
                    .iter()
                    .map(|action| self.menu_action_enabled(*action))
                    .collect();
                if let Some(index) = ui::popup::next_index(event, self.menu_index, &enabled) {
                    self.menu_index = index;
                    self.scroll_to_menu_selection();
                }
            }
        }
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
        if self.menu_page != Page::ConnectionInfo {
            if self.menu_page == Page::Display {
                popup = popup.child(
                    div()
                        .p(px(10.))
                        .text_size(px(11.))
                        .text_color(rgb(ui::colors().muted))
                        .child(self.display_details()),
                );
            }
            for (index, action) in self.menu_rows() {
                let (label, icon) = match action {
                    Action::Dynamic => (
                        i18n_key::RDP_DISPLAY_DYNAMIC,
                        if self.zoom == Zoom::Dynamic {
                            Icon::Check
                        } else {
                            Icon::Desktop
                        },
                    ),
                    Action::Fit => (
                        i18n_key::RDP_DISPLAY_FIT,
                        if self.zoom == Zoom::Fit {
                            Icon::Check
                        } else {
                            Icon::Desktop
                        },
                    ),
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
                        .when_some(action.help(), |row, hint| {
                            row.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(hint).to_owned())).into()
                            })
                        })
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
        let mut rows = vec![
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
        if let Some(quality) = &self.quality {
            rows.extend(quality.rows());
        }
        rows.push((
            i18n_key::RDP_CLIPBOARD_CONTENT,
            tr(if self.config.clipboard {
                i18n_key::RDP_CLIPBOARD_ENABLED
            } else {
                i18n_key::RDP_SHARED_CLIPBOARD_IS_DISABLED_FOR_THIS_CONNECTION
            })
            .into(),
        ));
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
                        this.scroll_to_menu_selection();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_menu_only_shows_cancel_while_files_can_be_cancelled() {
        let cancel = |can_cancel| {
            Page::Actions
                .rows(can_cancel)
                .position(|(_, action)| matches!(action, Action::CancelFiles))
        };
        assert!(cancel(true).is_some());
        assert_eq!(cancel(false), None);

        let copy = |can_cancel| {
            Page::Actions
                .rows(can_cancel)
                .enumerate()
                .find_map(|(row, (selection, action))| {
                    matches!(action, Action::CopyRemote).then_some((row, selection))
                })
                .unwrap()
        };
        let (active_row, selected) = copy(true);
        assert_eq!(copy(false), (active_row - 1, selected));
    }

    #[test]
    fn only_display_choices_have_explanatory_tooltips() {
        for action in Page::Actions.actions() {
            assert_eq!(action.help(), None);
        }
        for action in Page::Display.actions() {
            assert!(action.help().is_some());
        }
    }
}
