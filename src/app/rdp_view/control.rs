use super::*;
use crate::app::input_capture::{CaptureStatus, InputCapture};
use rustterm::i18n::messages as i18n_key;
use std::time::Duration;

pub(super) enum KeyboardControl {
    Released,
    Pending(InputCapture),
    Active(InputCapture),
    Limited(Message),
}

impl RdpView {
    pub fn captures_keyboard(&self) -> bool {
        !matches!(self.control, KeyboardControl::Released)
    }

    pub fn contains_desktop(&self, position: Point<Pixels>) -> bool {
        self.surface_visible() && self.remote_position(position, false).is_some()
    }

    pub fn release_control(&mut self, cx: &mut Context<Self>) {
        let held = self.captures_keyboard();
        self.drop_control();
        if held {
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }

    pub(super) fn drop_control(&mut self) {
        self.control = KeyboardControl::Released;
        self.capture_task = None;
        self.release_gesture.reset();
        self.physical_buttons.clear();
        self.release_input();
    }

    pub(super) fn remote_input_enabled(&self) -> bool {
        self.input_enabled()
            && matches!(
                self.control,
                KeyboardControl::Active(_) | KeyboardControl::Limited(_)
            )
    }

    pub(super) fn system_keys_captured(&self) -> bool {
        matches!(self.control, KeyboardControl::Active(_))
    }

    pub(super) fn has_native_capture(&self) -> bool {
        matches!(
            self.control,
            KeyboardControl::Pending(_) | KeyboardControl::Active(_)
        )
    }

    pub(super) fn interrupt_release_gesture(&mut self) {
        self.release_gesture.interrupt();
        if let KeyboardControl::Pending(capture) | KeyboardControl::Active(capture) =
            &mut self.control
        {
            capture.interrupt_release_gesture();
        }
    }

    pub(super) fn update_release_gate(&mut self) {
        let blocked = !self.physical_buttons.is_empty() || !self.preedit.is_empty();
        if let KeyboardControl::Pending(capture) | KeyboardControl::Active(capture) =
            &mut self.control
        {
            capture.set_release_blocked(blocked);
        }
        if blocked {
            self.release_gesture.interrupt();
        }
    }

    pub(super) fn request_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.captures_keyboard() || !self.input_enabled() || !window.is_window_active() {
            return;
        }
        if window.modifiers().modified() || !self.preedit.is_empty() {
            self.notice =
                Some(i18n_key::RDP_RELEASE_HELD_KEYS_THEN_CLICK_THE_REMOTE_DESKTOP_TO_TAKE);
            cx.notify();
            return;
        }
        self.focus.focus(window);
        self.clipboard.begin_control();
        self.physical_modifiers = window.modifiers();
        self.release_gesture.reset();
        self.notice = None;
        self.control = match InputCapture::request(window) {
            Ok(capture) => KeyboardControl::Pending(capture),
            Err(error) => {
                self.window_state
                    .record("rdp-keyboard-capture-unavailable", self.index);
                KeyboardControl::Limited(Message::from_error(error))
            }
        };
        self.update_release_gate();
        cx.emit(PaneEvent::Activity);
        cx.notify();
        if matches!(self.control, KeyboardControl::Pending(_)) {
            self.capture_task = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    let keep = cx.update(|window, cx| {
                        this.update(cx, |this, cx| this.poll_control(window, cx))
                    });
                    if !matches!(keep, Ok(Ok(true))) {
                        break;
                    }
                }
            }));
        }
    }

    pub(super) fn poll_control(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.input_enabled() || !window.is_window_active() || !self.focus.is_focused(window) {
            self.release_control(cx);
            return false;
        }
        let (status, released, keys_held) = match &mut self.control {
            KeyboardControl::Pending(capture) | KeyboardControl::Active(capture) => (
                capture.poll_status(),
                capture.take_release_request(),
                capture.has_held_keys(),
            ),
            _ => return false,
        };
        if released {
            self.release_control(cx);
            return false;
        }
        match status {
            CaptureStatus::Active
                if !keys_held && matches!(self.control, KeyboardControl::Pending(_)) =>
            {
                let KeyboardControl::Pending(capture) =
                    std::mem::replace(&mut self.control, KeyboardControl::Released)
                else {
                    unreachable!()
                };
                self.control = KeyboardControl::Active(capture);
                cx.emit(PaneEvent::Activity);
                cx.notify();
            }
            CaptureStatus::Denied => {
                let active = self.system_keys_captured();
                self.drop_control();
                if active {
                    self.notice = Some(
                        i18n_key::RDP_THE_DESKTOP_RELEASED_KEYBOARD_CONTROL_CLICK_THE_REMOTE_DESKTOP_TO,
                    );
                } else {
                    self.control = KeyboardControl::Limited(Message::new(
                        i18n_key::RDP_THE_DESKTOP_DID_NOT_GRANT_EXCLUSIVE_KEYBOARD_CONTROL,
                    ));
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
                return false;
            }
            _ => {}
        }
        true
    }

    pub(super) fn control_status(&self) -> (&'static str, u32) {
        match &self.control {
            KeyboardControl::Released => (
                i18n_key::RDP_CLICK_THE_REMOTE_DESKTOP_TO_TAKE_CONTROL,
                ui::MUTED,
            ),
            KeyboardControl::Pending(_) => (
                i18n_key::RDP_REQUESTING_KEYBOARD_CONTROL_CTRL_ALT_RELEASES,
                ui::WARNING,
            ),
            KeyboardControl::Active(_) => (
                i18n_key::RDP_CONTROLLING_REMOTE_DESKTOP_CTRL_ALT_RELEASES,
                ui::ACCENT,
            ),
            KeyboardControl::Limited(_) => (
                i18n_key::RDP_SYSTEM_SHORTCUTS_STAY_LOCAL_CTRL_ALT_RELEASES,
                ui::WARNING,
            ),
        }
    }

    pub(super) fn control_details(&self) -> String {
        match &self.control {
            KeyboardControl::Limited(reason) => format!(
                "{}\n{reason}",
                tr(i18n_key::RDP_EXCLUSIVE_KEYBOARD_CONTROL_IS_UNAVAILABLE_ORDINARY_INPUT_GOES_TO_THE)
            ),
            _ => tr(i18n_key::RDP_CONTROL_RELEASE_HELP).to_owned(),
        }
    }
}
