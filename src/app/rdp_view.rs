//! Native remote desktop pane. Frames are latest-value snapshots, never a queue.
//! The connection and input protocol remain independent of terminal sessions.
use super::{interaction::WindowState, layout::Axis, pane::PaneEvent, ui};
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Modifiers, Pixels, Point, RenderImage,
    Subscription, Task, Window, prelude::*,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{RdpConfig, Secret, Settings},
    i18n::{Message, tr},
    rdp::{
        RdpClient, RdpClipboardAction, RdpClipboardEvent, RdpCommand, RdpEvent, RdpFrame,
        RdpMouseButton, RdpSender,
    },
};
use std::{
    collections::BTreeSet,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::watch;

mod clipboard;
mod connection;
mod control;
mod file_clipboard;
mod input;
mod menu;
mod performance;
mod remote_app;
mod remote_files;
mod render;

pub struct RdpLaunch {
    pub config: RdpConfig,
    pub secret: Secret,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    WaitingCertificate,
    Connected,
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Zoom {
    #[default]
    Fit,
    Actual,
}

struct FileClipboardRead {
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for FileClipboardRead {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct FileTransferStatus {
    offer_id: u64,
    files: usize,
    bytes_sent: u64,
    available: bool,
}

impl FileTransferStatus {
    fn text(&self) -> String {
        rustterm::trf!(
            i18n_key::RDP_SENT_FILE_COUNT,
            files = self.files,
            size = super::file_transfer::format_bytes(self.bytes_sent)
        )
    }
}

pub struct RdpView {
    index: usize,
    title: String,
    pane_close_visible: bool,
    config: RdpConfig,
    window_state: WindowState,
    focus: FocusHandle,
    menu_focus: FocusHandle,
    menu_open: bool,
    menu_page: menu::Page,
    menu_index: usize,
    menu_scroll: gpui::ScrollHandle,
    state: State,
    applications: remote_app::Applications,
    sender: Option<RdpSender>,
    frames: Option<watch::Receiver<Option<Arc<RdpFrame>>>>,
    images: Vec<render::TileImage>,
    image_size: (u16, u16),
    image_acknowledgement: Option<rustrdp::FrameToken>,
    scheduled_acknowledgement: Option<rustrdp::FrameToken>,
    presentation_generation: u64,
    performance: performance::Performance,
    hide_local_pointer: bool,
    desktop_size: (u16, u16),
    negotiated_color_depth: Option<u16>,
    desktop_scale: u32,
    device_scale: f32,
    bounds: Option<Bounds<Pixels>>,
    layout_size: (f32, f32),
    last_resize: Option<(u16, u16)>,
    zoom: Zoom,
    pan: (f32, f32),
    visible: bool,
    unread: bool,
    closed: bool,
    error: Option<Message>,
    notice: Option<&'static str>,
    clipboard: clipboard::ClipboardState,
    file_clipboard_read: Option<FileClipboardRead>,
    file_transfer: Option<FileTransferStatus>,
    file_export_snapshot:
        Option<Task<Result<file_clipboard::export::Snapshot, file_clipboard::export::ExportError>>>,
    incoming_files: Option<remote_files::Transfer>,
    content_publication: Option<remote_files::ContentPublication>,
    audio_muted: bool,
    microphone_enabled: bool,
    retry_allowed: bool,
    control: control::KeyboardControl,
    capture_task: Option<Task<()>>,
    release_gesture: super::input_capture::release_gesture::ReleaseGesture,
    physical_modifiers: Modifiers,
    physical_buttons: Vec<gpui::MouseButton>,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: Vec<RdpMouseButton>,
    last_pointer: (u16, u16),
    wheel_remainder: (f32, f32),
    preedit: String,
    preedit_selection: Range<usize>,
    _subscriptions: Vec<Subscription>,
    _visibility_task: Task<()>,
    event_task: Option<Task<()>>,
    frame_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
}

impl EventEmitter<PaneEvent> for RdpView {}

impl RdpView {
    pub fn new(
        index: usize,
        title: String,
        launch: RdpLaunch,
        _settings: &Settings,
        window_state: WindowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let menu_focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_app_quit(|this: &mut Self, _| {
                this.drop_control();
                async {}
            }),
            cx.on_blur(&menu_focus, window, |this: &mut Self, _, cx| {
                if std::mem::take(&mut this.menu_open) {
                    cx.emit(PaneEvent::Activity);
                    cx.notify();
                }
            }),
            cx.on_focus(&focus, window, |this: &mut Self, window, cx| {
                if this.window_state.is_locked() {
                    return;
                }
                this.physical_modifiers = window.modifiers();
                this.unread = false;
                cx.emit(PaneEvent::Focused(this.index));
                cx.notify();
            }),
            cx.on_blur(&focus, window, |this: &mut Self, window, cx| {
                this.release_control(cx);
                this.physical_buttons.clear();
                // Window deactivation also emits blur; keep pending copies when
                // switching to the local file manager without changing panes.
                if !this.focus.is_focused(window) {
                    this.clipboard.cancel_export();
                }
                cx.notify();
            }),
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if this.window_state.is_locked() {
                    this.release_control(cx);
                    return;
                }
                this.physical_modifiers = window.modifiers();
                if !window.is_window_active() {
                    if this.menu_focus.is_focused(window) {
                        this.focus.focus(window);
                    }
                    this.menu_open = false;
                    this.release_control(cx);
                    this.physical_buttons.clear();
                } else {
                    // Let Wayland deliver the new selection offer before reading
                    // it; a clipboard result that arrived while inactive stays cached.
                    cx.on_next_frame(window, |this, window, cx| {
                        if this.sync_remote_clipboard(window, cx) {
                            cx.emit(PaneEvent::Activity);
                        }
                    });
                }
                // Restoring a window presents the latest snapshot even when the
                // server has become idle since the window was hidden.
                if this.visible {
                    cx.notify();
                }
            }),
            cx.on_release_in(window, |this: &mut Self, window, cx| {
                this.close_application_windows(cx);
                this.drop_control();
                if let Some(sender) = this.sender.take() {
                    sender.close();
                }
                this.clear_images(window);
                this.performance.flush(&this.window_state, this.index);
            }),
        ];
        let mut visibility = window_state.visibility.subscribe();
        let visibility_task = cx.spawn(async move |this, cx| {
            while visibility.changed().await.is_ok() {
                let visible = *visibility.borrow_and_update();
                if this
                    .update(cx, |this, cx| {
                        if this.closed {
                            return false;
                        }
                        this.invalidate_frame_acknowledgements();
                        // The X11 observer can report restoration after GPUI's
                        // MapNotify repaint. Wake the retained desktop even when
                        // the server is waiting for its outstanding frame ACKs.
                        if visible && this.surface_visible() {
                            cx.notify();
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
        let locked = window_state.is_locked();
        let mut view = Self {
            index,
            title,
            pane_close_visible: false,
            desktop_size: (
                launch.config.width
                    * u16::from(if launch.config.remote_app.is_some() {
                        1
                    } else {
                        launch.config.monitor_count
                    }),
                launch.config.height,
            ),
            config: launch.config.clone(),
            desktop_scale: launch
                .config
                .desktop_scale
                .unwrap_or_else(|| automatic_desktop_scale(window.scale_factor())),
            device_scale: window.scale_factor(),
            negotiated_color_depth: None,
            window_state,
            focus,
            menu_focus,
            menu_open: false,
            menu_page: menu::Page::Actions,
            menu_index: 0,
            menu_scroll: gpui::ScrollHandle::new(),
            state: State::Connecting,
            applications: remote_app::Applications::default(),
            sender: None,
            frames: None,
            images: Vec::new(),
            image_size: (0, 0),
            image_acknowledgement: None,
            scheduled_acknowledgement: None,
            presentation_generation: 0,
            performance: performance::Performance::default(),
            hide_local_pointer: false,
            bounds: None,
            layout_size: (0., 0.),
            last_resize: None,
            zoom: Zoom::Fit,
            pan: (0., 0.),
            visible: !locked,
            unread: false,
            closed: false,
            error: None,
            notice: None,
            clipboard: clipboard::ClipboardState::default(),
            file_clipboard_read: None,
            file_transfer: None,
            file_export_snapshot: None,
            incoming_files: None,
            content_publication: None,
            audio_muted: false,
            microphone_enabled: launch.config.audio_input,
            retry_allowed: false,
            control: control::KeyboardControl::Released,
            capture_task: None,
            release_gesture: super::input_capture::release_gesture::ReleaseGesture::default(),
            physical_modifiers: window.modifiers(),
            physical_buttons: Vec::new(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: Vec::new(),
            last_pointer: (0, 0),
            wheel_remainder: (0., 0.),
            preedit: String::new(),
            preedit_selection: 0..0,
            _subscriptions: subscriptions,
            _visibility_task: visibility_task,
            event_task: None,
            frame_task: None,
            resize_task: None,
        };
        view.connect(launch.secret, window, cx);
        if locked {
            view.set_locked(true, cx);
        }
        view
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn may_auto_reconnect(&self) -> bool {
        self.retry_allowed && !self.closed
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            State::Connecting | State::WaitingCertificate | State::Connected
        )
    }

    pub fn has_unread(&self) -> bool {
        self.unread
    }

    pub fn status_text(&self) -> String {
        tr(match self.state {
            State::Connecting => i18n_key::COMMON_CONNECTING_PROGRESS,
            State::WaitingCertificate => i18n_key::RDP_VERIFY_SERVER_CERTIFICATE,
            State::Connected => i18n_key::COMMON_CONNECTED,
            State::Disconnected => i18n_key::COMMON_DISCONNECTED,
            State::Failed => i18n_key::REDIS_CONNECTION_FAILED,
        })
        .into()
    }

    pub fn status_color(&self) -> u32 {
        match self.state {
            State::Connected => ui::colors().success,
            State::Connecting | State::WaitingCertificate => ui::colors().warning,
            State::Failed => ui::colors().danger,
            State::Disconnected => ui::colors().muted,
        }
    }

    pub fn status_details(&self) -> String {
        let mut detail = self.target();
        if self.state == State::Connected {
            detail.push_str(&format!(
                " · {} × {}",
                self.desktop_size.0, self.desktop_size.1
            ));
        }
        if let Some(error) = &self.error {
            detail.push_str(&format!(" · {error}"));
        }
        detail
    }

    pub fn diagnostic_state(&self) -> &'static str {
        match self.state {
            State::Connecting => "connecting",
            State::WaitingCertificate => "waiting-certificate",
            State::Connected => "connected",
            State::Disconnected => "disconnected",
            State::Failed => "failed",
        }
    }

    pub fn input_composing(&self, _: &App) -> bool {
        !self.preedit.is_empty()
    }

    pub fn set_pane_close_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.pane_close_visible != visible {
            self.pane_close_visible = visible;
            cx.notify();
        }
    }

    pub fn minimum_size(&self) -> (f32, f32) {
        (320., ui::PANE_HEADER_HEIGHT + 160.)
    }

    pub fn can_split(&self, axis: Axis) -> bool {
        let minimum = self.minimum_size();
        match axis {
            Axis::Horizontal => self.layout_size.0 >= minimum.0 * 2. + 4.,
            Axis::Vertical => self.layout_size.1 >= minimum.1 * 2. + 4.,
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            if !visible {
                self.invalidate_frame_acknowledgements();
                self.menu_open = false;
                self.clipboard.cancel_export();
                self.release_control(cx);
                self.resize_task = None;
            } else {
                self.request_resize(cx);
            }
            cx.notify();
        }
    }

    pub fn set_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.notify_application_lock(locked, cx);
        self.send(RdpCommand::SetLocked(locked), cx);
        if locked {
            self.invalidate_frame_acknowledgements();
            self.menu_open = false;
            self.release_control(cx);
            self.cancel_remote_file_receive(cx);
            let revision = self.clipboard.begin();
            self.clipboard.remote = None;
            self.clipboard.remote_content = None;
            self.clipboard.clear_files();
            self.send(
                RdpCommand::Clipboard {
                    revision,
                    action: RdpClipboardAction::CancelFiles,
                },
                cx,
            );
            self.resize_task = None;
        }
        if self.config.audio_output {
            self.send(RdpCommand::SetAudioMuted(locked || self.audio_muted), cx);
        }
        cx.notify();
    }

    pub fn apply_settings(&mut self, _: &Settings, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.close_application_windows(cx);
        self.disconnect();
        self.closed = true;
        self.event_task = None;
        self.frame_task = None;
        self.resize_task = None;
        self.frames = None;
        // on_release_in removes the texture with access to the current window.
        cx.notify();
    }

    pub fn disconnect_or_reconnect(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            self.close_application_windows(cx);
            self.disconnect();
            self.resize_task = None;
            cx.emit(PaneEvent::Activity);
            cx.notify();
        } else {
            cx.emit(PaneEvent::Reconnect(self.index));
        }
    }

    pub fn set_layout_size(&mut self, size: (f32, f32), cx: &mut Context<Self>) {
        if self.layout_size != size {
            self.layout_size = size;
            self.request_resize(cx);
            cx.notify();
        }
    }

    fn target(&self) -> String {
        let host = if self.config.host.contains(':')
            && !(self.config.host.starts_with('[') && self.config.host.ends_with(']'))
        {
            format!("[{}]", self.config.host)
        } else {
            self.config.host.clone()
        };
        let user = match self.config.domain.as_deref().filter(|s| !s.is_empty()) {
            Some(domain) => format!("{domain}\\{}", self.config.username),
            None => self.config.username.clone(),
        };
        format!("{user}@{host}:{}", self.config.port)
    }

    fn surface_visible(&self) -> bool {
        self.visible && !self.window_state.is_locked() && self.window_state.visibility.is_visible()
    }

    fn input_enabled(&self) -> bool {
        !self.window_state.is_locked()
            && (self.surface_visible()
                || (self.config.remote_app.is_some() && self.applications.active.is_some()))
            && !self.menu_open
            && self.state == State::Connected
            && !self.window_state.prompts.is_busy()
            && !self.closed
    }

    fn send(&mut self, command: RdpCommand, cx: &mut Context<Self>) -> bool {
        if self.window_state.is_locked()
            && !matches!(
                &command,
                RdpCommand::ReleaseAll
                    | RdpCommand::SetLocked(_)
                    | RdpCommand::SetAudioMuted(true)
                    | RdpCommand::CancelReceiveFiles { .. }
                    | RdpCommand::Clipboard {
                        action: RdpClipboardAction::Cancel | RdpClipboardAction::CancelFiles,
                        ..
                    }
            )
        {
            return false;
        }
        if matches!(
            command,
            RdpCommand::Key { pressed: true, .. }
                | RdpCommand::MouseButton { pressed: true, .. }
                | RdpCommand::Unicode(_)
                | RdpCommand::Wheel { .. }
                | RdpCommand::Resize { .. }
                | RdpCommand::ReleaseAll
                | RdpCommand::PasteRemote
        ) {
            let pending = self.file_clipboard_read.is_some();
            self.cancel_file_clipboard_read();
            if pending {
                cx.emit(PaneEvent::Activity);
                cx.notify();
            }
        }
        let Some(sender) = &self.sender else {
            return false;
        };
        if let Err(error) = sender.try_send(command) {
            self.window_state
                .record(error.diagnostic_code(), self.index);
            if error == rustterm::rdp::RdpError::FileSelection {
                self.clipboard.feedback = Some(clipboard::Feedback::Rejected(error));
                cx.emit(PaneEvent::Activity);
                cx.notify();
                return false;
            }
            if error == rustterm::rdp::RdpError::InputBusy {
                // The mailbox reserves a release after accepted input. Release
                // local capture too, so a rejected key cannot leave held state
                // or an unfinished clipboard gesture on either side.
                self.drop_control();
                self.clipboard.begin();
                self.clipboard.clear_files();
                self.file_export_snapshot = None;
                self.notice = Some(i18n_key::RDP_RDP_INPUT_IS_TEMPORARILY_BUSY_PLEASE_RETRY);
                cx.emit(PaneEvent::Activity);
                cx.notify();
                return false;
            }
            sender.close();
            self.sender = None;
            self.performance.flush(&self.window_state, self.index);
            self.state = State::Failed;
            self.error = Some(Message::from_error(error));
            self.drop_control();
            self.notice = None;
            self.clipboard = clipboard::ClipboardState::default();
            self.file_transfer = None;
            self.incoming_files = None;
            self.file_export_snapshot = None;
            cx.emit(PaneEvent::Activity);
            cx.notify();
            return false;
        }
        true
    }

    fn disconnect(&mut self) {
        self.content_publication = None;
        self.invalidate_frame_acknowledgements();
        self.performance.flush(&self.window_state, self.index);
        self.retry_allowed = false;
        self.menu_open = false;
        self.drop_control();
        if let Some(sender) = self.sender.take() {
            sender.close();
        }
        self.state = State::Disconnected;
        self.notice = None;
        self.clipboard = clipboard::ClipboardState::default();
        self.file_transfer = None;
        self.incoming_files = None;
        self.file_export_snapshot = None;
    }

    fn request_resize(&mut self, cx: &mut Context<Self>) {
        // Returning to the last sent size must also cancel an intermediate size
        // still waiting in the debounce timer.
        self.resize_task = None;
        if !self.config.uses_dynamic_resolution()
            || self.state != State::Connected
            || !self.visible
            || self.window_state.is_locked()
        {
            return;
        }
        let Some(requested) = resize_dimensions(self.layout_size, self.device_scale) else {
            return;
        };
        if self.last_resize == Some(requested) {
            return;
        }
        // Replace a pending resize instead of queueing every pixel of a drag.
        // This timer exists only while the viewport is actually changing.
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(std::time::Duration::from_millis(120)).await;
            let _ = this.update(cx, |this, cx| {
                if this.visible
                    && !this.window_state.is_locked()
                    && this.state == State::Connected
                    && this.send(
                        RdpCommand::Resize {
                            width: requested.0,
                            height: requested.1,
                        },
                        cx,
                    )
                {
                    this.last_resize = Some(requested);
                }
            });
        }));
    }
}

fn automatic_desktop_scale(device_scale: f32) -> u32 {
    (device_scale * 100.).round().clamp(100., 500.) as u32
}

fn resize_dimensions((width, height): (f32, f32), device_scale: f32) -> Option<(u16, u16)> {
    (width > 0. && height > ui::PANE_HEADER_HEIGHT).then(|| {
        (
            (width * device_scale).round().clamp(320., 3840.) as u16 & !1,
            ((height - ui::PANE_HEADER_HEIGHT) * device_scale)
                .round()
                .clamp(200., 2160.) as u16,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_resize_excludes_one_connection_bar() {
        let header = ui::PANE_HEADER_HEIGHT;
        assert_eq!(
            resize_dimensions((1280., 720. + header), 1.),
            Some((1280, 720))
        );
        assert_eq!(
            resize_dimensions((1280., 680. + header), 1.),
            Some((1280, 680))
        );
        assert_eq!(resize_dimensions((0., 720. + header), 1.), None);
        assert_eq!(resize_dimensions((1280., header), 1.), None);
    }

    #[test]
    fn hidpi_desktop_uses_physical_pixels_without_scaling_the_connection_bar() {
        assert_eq!(automatic_desktop_scale(1.25), 125);
        assert_eq!(automatic_desktop_scale(2.), 200);
        let height = 720. + ui::PANE_HEADER_HEIGHT;
        assert_eq!(resize_dimensions((1280., height), 2.), Some((2560, 1440)));
        assert_eq!(resize_dimensions((1281., height), 1.25), Some((1600, 900)));
        assert_eq!(
            resize_dimensions((2560., 1440. + ui::PANE_HEADER_HEIGHT), 2.),
            Some((3840, 2160))
        );
    }
}
