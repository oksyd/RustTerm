use super::clipboard::{ClipboardRead, Feedback};
use super::*;
use crate::app::text_input::alt_gr_text;
use gpui::{
    ClipboardEntry, ClipboardItem, EntityInputHandler, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, ModifiersChangedEvent, MouseButton, ScrollDelta, ScrollWheelEvent, UTF16Selection,
    point, px, size,
};
use rustterm::i18n::messages as i18n_key;

const MAX_TEXT_BYTES: usize = 64 * 1024;
type ModifierState = (u16, fn(Modifiers) -> bool);
const MODIFIERS: [ModifierState; 4] = [
    (0x1d, |m| m.control),
    (0x38, |m| m.alt),
    (0x2a, |m| m.shift),
    (0xe05b, |m| m.platform),
];

fn is_modifier(key: &str) -> bool {
    matches!(
        key,
        "control" | "ctrl" | "alt" | "altgr" | "shift" | "super" | "meta" | "cmd" | "fn"
    )
}

fn clipboard_text(item: Option<ClipboardItem>) -> ClipboardRead {
    let Some(item) = item else {
        return Ok(None);
    };
    let mut text: Option<String> = None;
    for entry in item.into_entries() {
        let ClipboardEntry::String(entry) = entry else {
            continue;
        };
        let value = entry.into_text();
        let existing = text.as_ref().map_or(0, String::len);
        if value.len() > rustterm::rdp::MAX_CLIPBOARD_BYTES.saturating_sub(existing) {
            return Err(i18n_key::RDP_CLIPBOARD_TEXT_IS_LIMITED_TO_1_MIB);
        }
        if value.contains('\0') {
            return Err(i18n_key::RDP_CLIPBOARD_TEXT_CANNOT_CONTAIN_NUL_CHARACTERS);
        }
        if let Some(text) = &mut text {
            text.push_str(&value);
        } else {
            text = Some(value);
        }
    }
    Ok(text)
}

/// Returns whether a local paste is forced, or None for an ordinary remote key.
fn paste_shortcut(key: &str, mods: Modifiers, clipboard: bool) -> Option<bool> {
    if mods.alt || mods.platform {
        return None;
    }
    if mods.control && key == "v" && (clipboard || mods.shift) {
        Some(mods.shift)
    } else if clipboard && mods.shift && !mods.control && key == "insert" {
        Some(false)
    } else {
        None
    }
}

fn modifier_changes(
    previous: Modifiers,
    current: Modifiers,
    remote_keys: &BTreeSet<u16>,
    forward_presses: bool,
) -> [Option<(u16, bool)>; 4] {
    MODIFIERS.map(|(scancode, active)| {
        let pressed = active(current);
        let held = remote_keys.contains(&scancode);
        // A local shortcut or Unicode commit may deliberately release a key
        // that is still physically held. Only a new press may restore it here.
        ((!pressed && held) || (pressed && !held && !active(previous) && forward_presses))
            .then_some((scancode, pressed))
    })
}

fn wheel_deltas(delta: ScrollDelta, remainder: &mut (f32, f32)) -> [(bool, i16); 2] {
    // GPUI's Linux backend emits three lines per detent; Windows expects 120 units.
    let (horizontal, vertical) = match delta {
        ScrollDelta::Lines(delta) => (-delta.x * 40., delta.y * 40.),
        ScrollDelta::Pixels(delta) => (-f32::from(delta.x) * 3., f32::from(delta.y) * 3.),
    };
    // GPUI deltas move content; Windows horizontal wheel units point to the right.
    // Keep independent fractions, and let rustrdp split the signed 9-bit wire events.
    let take = |remainder: &mut f32, delta: f32| {
        *remainder += delta;
        let units = remainder
            .trunc()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        *remainder -= f32::from(units);
        units
    };
    [
        (false, take(&mut remainder.1, vertical)),
        (true, take(&mut remainder.0, horizontal)),
    ]
}

fn local_pan_delta(delta: ScrollDelta, horizontal: bool) -> (f32, f32) {
    let delta = delta.pixel_delta(px(40.));
    // GPUI already maps Shift+wheel onto x. The explicit pan chord chooses the
    // destination axis; it is not a second Shift conversion for remote input.
    let distance = if delta.y != px(0.) { delta.y } else { delta.x };
    if horizontal {
        (f32::from(distance), 0.)
    } else {
        (0., f32::from(distance))
    }
}

impl RdpView {
    pub(super) fn release_input(&mut self) {
        self.cancel_file_clipboard_read();
        // A consumed paste may still be waiting for the remote clipboard ACK
        // even when no physical key is held. Blur cancels that operation too.
        if let Some(sender) = &self.sender
            && let Err(error) = sender.try_send(RdpCommand::ReleaseAll)
        {
            self.window_state
                .record(error.diagnostic_code(), self.index);
            if error != rustterm::rdp::RdpError::InputBusy {
                sender.close();
            }
        }
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        self.wheel_remainder = (0., 0.);
        self.preedit.clear();
        self.preedit_selection = 0..0;
    }

    fn sync_modifiers(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) -> bool {
        for (scancode, active) in MODIFIERS {
            let pressed = active(modifiers) && (scancode != 0xe05b || self.system_keys_captured());
            if self.pressed_keys.contains(&scancode) != pressed {
                if !self.send(RdpCommand::Key { scancode, pressed }, cx) {
                    return false;
                }
                if pressed {
                    self.pressed_keys.insert(scancode);
                } else {
                    self.pressed_keys.remove(&scancode);
                }
            }
        }
        true
    }

    pub(super) fn modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.is_focused(window) {
            return;
        }
        let modifiers = event.modifiers;
        let previous = std::mem::replace(&mut self.physical_modifiers, modifiers);
        if self.captures_keyboard() && !self.system_keys_captured() {
            if !self.preedit.is_empty() || !self.physical_buttons.is_empty() {
                self.release_gesture.interrupt();
            }
            if self.release_gesture.modifiers_changed(previous, modifiers) {
                self.release_control(cx);
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
            if !modifiers.modified() && self.preedit.is_empty() && self.physical_buttons.is_empty()
            {
                self.release_gesture.reset();
            }
        }
        if self.has_native_capture() {
            self.poll_control(window, cx);
        }
        let forward_presses =
            self.remote_input_enabled() && window.is_window_active() && self.preedit.is_empty();
        for (scancode, pressed) in
            modifier_changes(previous, modifiers, &self.pressed_keys, forward_presses)
                .into_iter()
                .flatten()
        {
            if scancode == 0xe05b && pressed && !self.system_keys_captured() {
                continue;
            }
            if !self.send(RdpCommand::Key { scancode, pressed }, cx) {
                break;
            }
            if pressed {
                self.pressed_keys.insert(scancode);
            } else {
                self.pressed_keys.remove(&scancode);
            }
        }
    }

    fn modifiers_released(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) -> bool {
        for (scancode, active) in MODIFIERS {
            if !active(modifiers)
                && self.pressed_keys.remove(&scancode)
                && !self.send(
                    RdpCommand::Key {
                        scancode,
                        pressed: false,
                    },
                    cx,
                )
            {
                return false;
            }
        }
        true
    }

    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked() {
            return;
        }
        if self.menu_key_down(event, window, cx) {
            return;
        }
        if !self.focus.is_focused(window) {
            return;
        }
        let key = &event.keystroke;
        let mods = key.modifiers;
        if !is_modifier(&key.key) && !self.system_keys_captured() {
            self.release_gesture.interrupt();
        }
        let paste = paste_shortcut(&key.key, mods, self.config.clipboard);
        if paste.is_some() {
            self.window_state.record("rdp-paste-shortcut", self.index);
        }
        if !self.preedit.is_empty() || self.window_state.prompts.is_busy() {
            if paste.is_some() {
                self.window_state.record(
                    if self.window_state.prompts.is_busy() {
                        "rdp-paste-blocked-prompt"
                    } else {
                        "rdp-paste-blocked-composition"
                    },
                    self.index,
                );
            }
            return;
        }
        // Local workspace shortcuts are available only after releasing control.
        if !self.captures_keyboard() && mods.control && !mods.alt && !mods.platform {
            let local = if mods.shift {
                match key.key.as_str() {
                    "t" => Some(PaneEvent::NewTab),
                    "w" => Some(PaneEvent::Close(self.index)),
                    "d" => Some(PaneEvent::Split(Axis::Horizontal)),
                    "e" => Some(PaneEvent::Split(Axis::Vertical)),
                    "j" => Some(PaneEvent::NextPane),
                    "enter" | "return" => Some(PaneEvent::Zoom),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(local) = local {
                self.release_input();
                cx.emit(local);
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
            if key.key == "tab" || (mods.shift && key.key == "b") {
                self.release_input();
                return; // Workspace handles tab order and the connection sidebar.
            }
        }
        if !self.remote_input_enabled() {
            if paste.is_some() {
                self.window_state.record("rdp-paste-inactive", self.index);
            }
            return;
        }
        if mods.platform && !self.system_keys_captured() {
            return;
        }
        if let Some(force_local) = paste {
            self.start_clipboard_action(true, force_local, window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if self.config.clipboard
            && mods.control
            && !mods.shift
            && !mods.alt
            && !mods.platform
            && matches!(key.key.as_str(), "c" | "x")
        {
            // Keep remote copy→paste intact, including a paste pressed before
            // the CLIPRDR response arrives. New Ubuntu text still takes priority.
            let Some(revision) = self.begin_clipboard(cx) else {
                return;
            };
            let local = self.read_clipboard(cx);
            if !self.send(
                RdpCommand::Clipboard {
                    revision,
                    action: RdpClipboardAction::Cancel,
                },
                cx,
            ) {
                return;
            }
            self.clipboard.expect_remote_copy(local);
            if self.config.clipboard {
                // Observe ownership from the copy gesture, before its network
                // round trip. A delayed FORMAT_LIST must not adopt a newer
                // Ubuntu selection as the baseline for this remote copy.
                self.file_export_snapshot = Some(
                    cx.background_executor()
                        .spawn(async { file_clipboard::export::Snapshot::capture() }),
                );
            }
        }
        // AltGr text must not be interpreted as a Ctrl+Alt remote shortcut.
        if let Some(text) = alt_gr_text(key) {
            // GPUI's Linux backends only forward text automatically when the
            // modifiers are a subset of Shift. Deliver AltGr text explicitly.
            self.commit_text(text, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        // Ordinary typing must reach the Windows keyboard/IME pipeline. Unicode
        // injection bypasses it, so even a remote Chinese IME receives Latin
        // text when the local desktop is using an English keyboard.
        if let Some((scancode, mods)) = key_scancode(key, window.modifiers()) {
            if self.sync_modifiers(mods, cx)
                && self.send(
                    RdpCommand::Key {
                        scancode,
                        pressed: true,
                    },
                    cx,
                )
            {
                self.pressed_keys.insert(scancode);
            }
            // Stops the platform from also delivering this as committed text.
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    pub(super) fn key_up(
        &mut self,
        event: &KeyUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(scancode) = scancode(&event.keystroke.key)
            && self.pressed_keys.remove(&scancode)
        {
            self.send(
                RdpCommand::Key {
                    scancode,
                    pressed: false,
                },
                cx,
            );
            cx.stop_propagation();
        }
        self.modifiers_released(window.modifiers(), cx);
        if !is_modifier(&event.keystroke.key)
            && !window.modifiers().modified()
            && self.preedit.is_empty()
            && self.physical_buttons.is_empty()
        {
            self.release_gesture.reset();
        }
    }

    pub(super) fn track_pointer_button(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            if !self.physical_buttons.contains(&button) {
                self.physical_buttons.push(button);
            }
        } else {
            self.physical_buttons.retain(|held| *held != button);
        }
        self.update_release_gate();
    }

    pub(super) fn mouse_button(
        &mut self,
        button: MouseButton,
        pressed: bool,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.track_pointer_button(button, pressed);
        if pressed {
            self.interrupt_release_gesture();
        }
        let button = match button {
            MouseButton::Left => RdpMouseButton::Left,
            MouseButton::Right => RdpMouseButton::Right,
            MouseButton::Middle => RdpMouseButton::Middle,
            _ => return,
        };
        if !self.remote_input_enabled() && pressed {
            return;
        }
        if !pressed && !self.pressed_buttons.contains(&button) {
            if !modifiers.modified() && self.preedit.is_empty() && self.physical_buttons.is_empty()
            {
                self.release_gesture.reset();
            }
            return;
        }
        let Some((x, y)) = self.remote_position(position, !pressed) else {
            return;
        };
        if self.remote_input_enabled() && self.preedit.is_empty() {
            if !self.sync_modifiers(modifiers, cx) {
                cx.stop_propagation();
                return;
            }
        } else if !self.modifiers_released(modifiers, cx) {
            cx.stop_propagation();
            return;
        }
        if self.send(
            RdpCommand::MouseButton {
                button,
                pressed,
                x,
                y,
            },
            cx,
        ) {
            self.last_pointer = (x, y);
            if pressed {
                if !self.pressed_buttons.contains(&button) {
                    self.pressed_buttons.push(button);
                }
            } else {
                self.pressed_buttons.retain(|held| *held != button);
            }
        }
        if !modifiers.modified() && self.preedit.is_empty() && self.physical_buttons.is_empty() {
            self.release_gesture.reset();
        }
        cx.stop_propagation();
    }

    pub(super) fn mouse_move(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.remote_input_enabled()
            && let Some((x, y)) = self.remote_position(position, !self.pressed_buttons.is_empty())
            && self.last_pointer != (x, y)
            && self.send(RdpCommand::Pointer { x, y }, cx)
        {
            self.last_pointer = (x, y);
        }
    }

    pub(super) fn scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.interrupt_release_gesture();
        if !event.modifiers.modified()
            && self.preedit.is_empty()
            && self.physical_buttons.is_empty()
        {
            self.release_gesture.reset();
        }
        if !self.captures_keyboard()
            && self.zoom == Zoom::Actual
            && event.modifiers.control
            && event.modifiers.shift
        {
            // A deliberate local gesture makes every part of a large desktop
            // reachable without stealing ordinary remote wheel events.
            self.release_input();
            let (dx, dy) = local_pan_delta(event.delta, event.modifiers.alt);
            if let Some(bounds) = self.bounds {
                self.pan.0 = (self.pan.0 - dx).clamp(
                    0.,
                    (f32::from(self.desktop_size.0) - f32::from(bounds.size.width)).max(0.),
                );
                self.pan.1 = (self.pan.1 - dy).clamp(
                    0.,
                    (f32::from(self.desktop_size.1) - f32::from(bounds.size.height)).max(0.),
                );
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if !self.remote_input_enabled() {
            return;
        }
        let Some((x, y)) = self.remote_position(event.position, false) else {
            return;
        };
        if !self.sync_modifiers(event.modifiers, cx) {
            cx.stop_propagation();
            return;
        }
        for (horizontal, delta) in wheel_deltas(event.delta, &mut self.wheel_remainder) {
            if delta != 0
                && !self.send(
                    RdpCommand::Wheel {
                        horizontal,
                        delta,
                        x,
                        y,
                    },
                    cx,
                )
            {
                break;
            }
        }
        cx.stop_propagation();
    }

    pub(super) fn send_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_state
            .record("rdp-clipboard-send-click", self.index);
        if self.config.clipboard && self.input_enabled() {
            self.start_clipboard_action(false, true, window, cx);
        }
    }

    pub(super) fn cancel_file_clipboard_read(&mut self) {
        self.file_clipboard_read = None;
        if matches!(self.clipboard.feedback, Some(Feedback::ReadingFiles)) {
            self.clipboard.feedback = None;
        }
    }

    fn start_clipboard_action(
        &mut self,
        paste: bool,
        force_local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked() {
            return;
        }
        // Keep an explicitly selected file offer only while the local clipboard
        // matches its selection-time snapshot. New local text takes precedence.
        if paste && !force_local && self.clipboard.has_selected_files() {
            self.release_input();
            let local = self.read_clipboard(cx);
            if self.clipboard.prefers_files(&local) {
                if self.clipboard.files_ready() {
                    self.send(RdpCommand::PasteRemote, cx);
                } else {
                    self.clipboard.feedback = Some(Feedback::Waiting);
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
                return;
            }
        }
        let Some(revision) = self.begin_clipboard(cx) else {
            return;
        };
        let local = if self.config.clipboard {
            self.read_clipboard(cx)
        } else {
            Err(i18n_key::RDP_SHARED_CLIPBOARD_IS_DISABLED_FOR_THIS_CONNECTION)
        };
        if !self.config.clipboard
            || (paste && !force_local && self.clipboard.prefers_remote(&local))
        {
            self.finish_text_paste(revision, local, paste, force_local, cx);
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let read_cancel = cancel.clone();
        let completion_cancel = cancel.clone();
        let files_enabled = self.config.clipboard_files;
        let pending = cx
            .background_executor()
            .spawn(async move { file_clipboard::read(read_cancel, files_enabled) });
        self.clipboard.feedback = Some(Feedback::ReadingFiles);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = pending.await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if completion_cancel.load(Ordering::Relaxed)
                        || this.clipboard.revision() != revision
                        || !this.input_enabled()
                        || !this.preedit.is_empty()
                        || !window.is_window_active()
                        || !this.focus.is_focused(window)
                    {
                        return;
                    }
                    this.file_clipboard_read = None;
                    if matches!(
                        result,
                        Ok(None) | Err(file_clipboard::ReadError::Unavailable)
                    ) && this.read_clipboard(cx) != local
                    {
                        this.clipboard.feedback = Some(Feedback::Message(
                            file_clipboard::ReadError::Changed.message(),
                        ));
                        cx.emit(PaneEvent::Activity);
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(Some(file_clipboard::SelectionData::Files(paths))) => {
                            this.publish_files(revision, paths, paste, false, cx)
                        }
                        Ok(Some(file_clipboard::SelectionData::Content(content))) => {
                            this.publish_content(revision, content, paste, cx)
                        }
                        Ok(None) => this.finish_text_paste(revision, local, paste, force_local, cx),
                        // A missing Xwayland bridge must not disable ordinary text paste.
                        Err(file_clipboard::ReadError::Unavailable)
                            if matches!(local, Ok(Some(_))) =>
                        {
                            this.finish_text_paste(revision, local, paste, force_local, cx);
                        }
                        Err(error) => {
                            this.clipboard.feedback = Some(Feedback::Message(error.message()));
                            cx.emit(PaneEvent::Activity);
                            cx.notify();
                        }
                    }
                })
            });
        });
        self.file_clipboard_read = Some(FileClipboardRead {
            cancel,
            _task: task,
        });
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }

    fn finish_text_paste(
        &mut self,
        revision: u64,
        local: ClipboardRead,
        paste: bool,
        force_local: bool,
        cx: &mut Context<Self>,
    ) {
        if paste && !force_local && self.clipboard.prefers_remote(&local) {
            self.release_input();
            if self.send(
                RdpCommand::Clipboard {
                    revision,
                    action: RdpClipboardAction::Cancel,
                },
                cx,
            ) {
                self.send(RdpCommand::PasteRemote, cx);
            }
            self.clipboard.feedback = None;
            cx.emit(PaneEvent::Activity);
            cx.notify();
        } else {
            self.publish_clipboard(revision, local, paste, cx);
        }
    }

    fn publish_content(
        &mut self,
        revision: u64,
        content: rustrdp::ClipboardContent,
        paste: bool,
        cx: &mut Context<Self>,
    ) {
        let action = if paste {
            RdpClipboardAction::PasteContent(content)
        } else {
            RdpClipboardAction::SendContent(content)
        };
        if self.send(RdpCommand::Clipboard { revision, action }, cx) {
            self.clipboard.publish();
            self.file_transfer = None;
            self.error = None;
            cx.emit(PaneEvent::Activity);
        }
        cx.notify();
    }

    fn publish_files(
        &mut self,
        revision: u64,
        paths: Vec<std::path::PathBuf>,
        paste: bool,
        selected: bool,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked()
            || !self.config.clipboard
            || !self.config.clipboard_files
            || paths.is_empty()
        {
            return;
        }
        let action = if paste {
            RdpClipboardAction::PasteFiles(paths)
        } else {
            RdpClipboardAction::SendFiles(paths)
        };
        if self.send(RdpCommand::Clipboard { revision, action }, cx) {
            let local = selected.then(|| self.read_clipboard(cx));
            self.clipboard.publish_files(local);
            self.error = None;
            self.file_transfer = None;
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }

    pub(super) fn choose_files(&mut self, directories: bool, cx: &mut Context<Self>) {
        if !self.config.clipboard || !self.config.clipboard_files || !self.input_enabled() {
            return;
        }
        let Some(revision) = self.begin_clipboard(cx) else {
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        self.release_control(cx);
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories,
            multiple: true,
            prompt: Some(tr(i18n_key::RDP_SELECT_FILES_TO_SEND_TO_THE_REMOTE_DESKTOP).into()),
        });
        cx.spawn(async move |this, cx| {
            let response = response.await;
            let valid = permit.is_valid();
            drop(permit);
            let _ = this.update(cx, |this, cx| {
                if !valid
                    || this.window_state.is_locked()
                    || this.closed
                    || this.state != State::Connected
                    || this.clipboard.revision() != revision
                {
                    return;
                }
                match response {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => {
                        this.publish_files(revision, paths, false, true, cx);
                    }
                    Ok(Ok(_)) => {}
                    _ => {
                        this.clipboard.feedback = Some(Feedback::Message(
                            i18n_key::COMMON_COULD_NOT_OPEN_THE_FILE_PICKER_PLEASE_TRY_AGAIN,
                        ));
                        cx.emit(PaneEvent::Activity);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn cancel_file_transfer(&mut self, cx: &mut Context<Self>) {
        self.cancel_remote_file_receive(cx);
        let Some(revision) = self.begin_clipboard(cx) else {
            return;
        };
        if self.send(
            RdpCommand::Clipboard {
                revision,
                action: RdpClipboardAction::CancelFiles,
            },
            cx,
        ) {
            self.clipboard.clear_files();
            self.clipboard.feedback =
                Some(Feedback::Message(i18n_key::COMMON_FILE_TRANSFER_CANCELLED));
            if let Some(transfer) = &mut self.file_transfer {
                transfer.available = false;
            }
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }

    pub(super) fn toggle_audio_mute(&mut self, cx: &mut Context<Self>) {
        let muted = !self.audio_muted;
        if !self.window_state.is_locked()
            && self.config.audio_output
            && self.state == State::Connected
            && self.send(RdpCommand::SetAudioMuted(muted), cx)
        {
            self.audio_muted = muted;
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }

    fn begin_clipboard(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        if self.window_state.is_locked() {
            return None;
        }
        self.cancel_remote_file_receive(cx);
        self.cancel_file_clipboard_read();
        let feedback_cleared = self.notice.is_some() || self.clipboard.feedback.is_some();
        self.notice = None;
        let previous = self.clipboard.revision();
        let revision = self.clipboard.begin();
        cx.notify();
        // Cancel before a potentially blocking OS read. Keep the worker on the
        // old revision until the new action is known, so events produced during
        // the read cannot acquire ownership of the forthcoming local paste.
        if !self.send(
            RdpCommand::Clipboard {
                revision: previous,
                action: RdpClipboardAction::Cancel,
            },
            cx,
        ) {
            return None;
        }
        if feedback_cleared {
            cx.emit(PaneEvent::Activity);
        }
        Some(revision)
    }

    pub(super) fn read_clipboard(&self, cx: &App) -> ClipboardRead {
        if self.window_state.is_locked() {
            return Ok(None);
        }
        self.window_state
            .record("rdp-clipboard-read-start", self.index);
        let result = clipboard_text(cx.read_from_clipboard());
        self.window_state.record(
            match &result {
                Ok(Some(_)) => "rdp-clipboard-read-text",
                Ok(None) => "rdp-clipboard-read-empty",
                Err(_) => "rdp-clipboard-read-rejected",
            },
            self.index,
        );
        result
    }

    pub(super) fn receive_clipboard_offer(
        &mut self,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explicit = self.clipboard.awaiting_copy_offer();
        let Some(eligible) = self.clipboard.remote_offer(revision) else {
            return;
        };
        let snapshot = if explicit {
            self.file_export_snapshot.take()
        } else {
            None
        };
        self.cancel_remote_file_receive(cx);
        if self.window_state.is_locked()
            || !eligible
            || !self.config.clipboard
            || !self.visible
            || self.closed
            || self.state != State::Connected
            || self.menu_open
            || self.window_state.prompts.is_busy()
            || !self.focus.is_focused(window)
        {
            self.clipboard.cancel_export();
            return;
        }
        // Wayland may temporarily expose no text when our window is inactive.
        // In that case the retained native observer protects ownership instead.
        if self.clipboard.has_export()
            && (window.is_window_active() || is_x11())
            && self
                .clipboard
                .local_changed_since_copy(&self.read_clipboard(cx))
        {
            self.clipboard.cancel_export();
            return;
        }
        if !self.clipboard.has_export() && window.is_window_active() {
            let local = self.read_clipboard(cx);
            self.clipboard.capture_export(local);
        }
        if self.config.clipboard && self.clipboard.has_export() {
            self.file_export_snapshot = if explicit {
                snapshot
            } else if window.is_window_active() {
                Some(
                    cx.background_executor()
                        .spawn(async { file_clipboard::export::Snapshot::capture() }),
                )
            } else {
                None
            };
        }
    }

    pub(super) fn sync_remote_clipboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.window_state.is_locked()
            || !self.config.clipboard
            || !self.visible
            || self.closed
            || self.state != State::Connected
            || self.menu_open
            || self.window_state.prompts.is_busy()
            || !self.focus.is_focused(window)
            || (!window.is_window_active() && !is_x11())
            || !self.clipboard.export_ready()
        {
            return false;
        }
        let local = self.read_clipboard(cx);
        if let Some(text) = self.clipboard.take_export(&local) {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            self.clipboard.remote_copy(Ok(Some(text)));
            let feedback_cleared = self.clipboard.feedback.take().is_some();
            self.window_state
                .record("rdp-clipboard-received", self.index);
            return feedback_cleared;
        }
        false
    }

    fn publish_clipboard(
        &mut self,
        revision: u64,
        local: ClipboardRead,
        paste: bool,
        cx: &mut Context<Self>,
    ) {
        match local {
            Ok(Some(text)) => {
                let action = if paste {
                    RdpClipboardAction::PasteText(text)
                } else {
                    RdpClipboardAction::SendText(text)
                };
                if self.send(RdpCommand::Clipboard { revision, action }, cx) {
                    if paste {
                        self.window_state
                            .record("rdp-clipboard-paste-queued", self.index);
                    }
                    self.clipboard.publish();
                    self.file_transfer = None;
                    self.error = None;
                    cx.emit(PaneEvent::Activity);
                }
            }
            result => {
                self.release_input();
                if self.send(
                    RdpCommand::Clipboard {
                        revision,
                        action: RdpClipboardAction::Cancel,
                    },
                    cx,
                ) {
                    self.clipboard.feedback =
                        Some(Feedback::Message(result.err().unwrap_or(
                            i18n_key::RDP_THE_LOCAL_CLIPBOARD_CONTAINS_NO_TEXT,
                        )));
                    cx.emit(PaneEvent::Activity);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn copy_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.config.clipboard
            && self.input_enabled()
            && let Some(content) = self.clipboard.remote_content.clone()
        {
            let Some(revision) = self.begin_clipboard(cx) else {
                return;
            };
            self.file_export_snapshot = Some(
                cx.background_executor()
                    .spawn(async { file_clipboard::export::Snapshot::capture() }),
            );
            self.publish_remote_content(revision, content, cx);
            return;
        }
        if self.config.clipboard
            && self.input_enabled()
            && let Some(text) = self.clipboard.remote.clone()
        {
            let Some(revision) = self.begin_clipboard(cx) else {
                return;
            };
            if !self.send(
                RdpCommand::Clipboard {
                    revision,
                    action: RdpClipboardAction::Cancel,
                },
                cx,
            ) {
                return;
            }
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            self.clipboard.remote_copy(Ok(Some(text)));
            self.clipboard.feedback = Some(Feedback::Message(
                i18n_key::RDP_REMOTE_TEXT_COPIED_TO_THE_LOCAL_CLIPBOARD,
            ));
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
    }

    pub(super) fn request_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.config.clipboard && self.input_enabled() {
            let Some(revision) = self.begin_clipboard(cx) else {
                return;
            };
            if self.send(
                RdpCommand::Clipboard {
                    revision,
                    action: RdpClipboardAction::RequestText,
                },
                cx,
            ) {
                self.clipboard.feedback = Some(Feedback::Requesting);
                cx.emit(PaneEvent::Activity);
            }
            cx.notify();
        }
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return;
        }
        if !self.sync_modifiers(Modifiers::default(), cx) {
            return;
        }
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(4096);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            if !self.send(RdpCommand::Unicode(rest[..end].to_owned()), cx) {
                break;
            }
            rest = &rest[end..];
        }
    }
}

impl EntityInputHandler for RdpView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = utf16_range(&self.preedit, range);
        *adjusted = Some(
            self.preedit[..bytes.start].encode_utf16().count()
                ..self.preedit[..bytes.end].encode_utf16().count(),
        );
        Some(self.preedit[bytes].into())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.preedit_selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.preedit.is_empty()).then(|| 0..self.preedit.encode_utf16().count())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preedit.clear();
        self.preedit_selection = 0..0;
        self.update_release_gate();
        if !window.modifiers().modified() && self.physical_buttons.is_empty() {
            self.release_gesture.reset();
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let had_preedit = !self.preedit.is_empty();
        self.preedit.clear();
        self.preedit_selection = 0..0;
        if had_preedit {
            self.update_release_gate();
        }
        if self.remote_input_enabled() && self.focus.is_focused(window) {
            self.release_gesture.interrupt();
            self.commit_text(text, cx);
            if !window.modifiers().modified() && self.physical_buttons.is_empty() {
                self.release_gesture.reset();
            }
        }
        if had_preedit {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.remote_input_enabled() || !self.focus.is_focused(window) {
            return;
        }
        self.release_gesture.interrupt();
        if self.preedit.is_empty() && !text.is_empty() {
            self.release_input();
        }
        let bytes = range
            .map(|range| utf16_range(&self.preedit, range))
            .unwrap_or(0..self.preedit.len());
        if self.preedit.len() - bytes.len() + text.len() > 8192
            || text.chars().any(char::is_control)
        {
            return;
        }
        let start = self.preedit[..bytes.start].encode_utf16().count();
        self.preedit.replace_range(bytes, text);
        let length = text.encode_utf16().count();
        self.preedit_selection = selected
            .map(|range| {
                start + range.start.min(length)
                    ..start + range.end.min(length).max(range.start.min(length))
            })
            .unwrap_or(start + length..start + length);
        self.update_release_gate();
        if self.preedit.is_empty()
            && !window.modifiers().modified()
            && self.physical_buttons.is_empty()
        {
            self.release_gesture.reset();
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            point(element.left() + px(8.), element.bottom() - px(26.)),
            size(px(1.), px(20.)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.preedit_selection.end)
    }
}

fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    fn offset(text: &str, target: usize) -> usize {
        let mut units = 0;
        for (byte, ch) in text.char_indices() {
            if units >= target {
                return byte;
            }
            units += ch.len_utf16();
        }
        text.len()
    }
    let start = offset(text, range.start);
    start..offset(text, range.end).max(start)
}

fn key_scancode(key: &Keystroke, physical: Modifiers) -> Option<(u16, Modifiers)> {
    if alt_gr_text(key).is_some() {
        return None;
    }
    let mut modifiers = key.modifiers;
    // GPUI removes Shift from symbol keystrokes (Shift+1 becomes "!").
    // Restore it without changing local shortcut matching above.
    modifiers.shift |= physical.shift;
    let shortcut = modifiers.control || modifiers.alt || modifiers.platform;
    if !shortcut
        && key.key_char.as_deref().is_some_and(|text| {
            // Keep local layout/compose text on the Unicode path when the key
            // cannot reproduce it. GPUI may expose an ASCII key name for a
            // non-Latin layout, and does not forward Caps Lock on every backend.
            if text.len() != 1 || !text.is_ascii() {
                return true;
            }
            let character = text.as_bytes()[0];
            let shifted =
                character.is_ascii_uppercase() || b"~!@#$%^&*()_+{}|:\"<>?".contains(&character);
            character != b' ' && shifted != modifiers.shift
        })
    {
        return None;
    }
    Some((scancode(&key.key)?, modifiers))
}

/// Set-1 scan codes; the high E0 byte marks extended RDP keys.
fn scancode(key: &str) -> Option<u16> {
    Some(match key {
        "escape" => 0x01,
        "1" | "!" => 0x02,
        "2" | "@" => 0x03,
        "3" | "#" => 0x04,
        "4" | "$" => 0x05,
        "5" | "%" => 0x06,
        "6" | "^" => 0x07,
        "7" | "&" => 0x08,
        "8" | "*" => 0x09,
        "9" | "(" => 0x0a,
        "0" | ")" => 0x0b,
        "-" | "_" => 0x0c,
        "=" | "+" => 0x0d,
        "backspace" => 0x0e,
        "tab" => 0x0f,
        "q" => 0x10,
        "w" => 0x11,
        "e" => 0x12,
        "r" => 0x13,
        "t" => 0x14,
        "y" => 0x15,
        "u" => 0x16,
        "i" => 0x17,
        "o" => 0x18,
        "p" => 0x19,
        "[" | "{" => 0x1a,
        "]" | "}" => 0x1b,
        "enter" | "return" => 0x1c,
        "control" => 0x1d,
        "a" => 0x1e,
        "s" => 0x1f,
        "d" => 0x20,
        "f" => 0x21,
        "g" => 0x22,
        "h" => 0x23,
        "j" => 0x24,
        "k" => 0x25,
        "l" => 0x26,
        ";" | ":" => 0x27,
        "'" | "\"" => 0x28,
        "`" | "~" => 0x29,
        "shift" => 0x2a,
        "\\" | "|" => 0x2b,
        "z" => 0x2c,
        "x" => 0x2d,
        "c" => 0x2e,
        "v" => 0x2f,
        "b" => 0x30,
        "n" => 0x31,
        "m" => 0x32,
        "," | "<" => 0x33,
        "." | ">" => 0x34,
        "/" | "?" => 0x35,
        "alt" => 0x38,
        "space" | " " => 0x39,
        "capslock" => 0x3a,
        "f1" => 0x3b,
        "f2" => 0x3c,
        "f3" => 0x3d,
        "f4" => 0x3e,
        "f5" => 0x3f,
        "f6" => 0x40,
        "f7" => 0x41,
        "f8" => 0x42,
        "f9" => 0x43,
        "f10" => 0x44,
        "numlock" => 0x45,
        "scrolllock" => 0x46,
        "f11" => 0x57,
        "f12" => 0x58,
        "home" => 0xe047,
        "up" => 0xe048,
        "pageup" => 0xe049,
        "left" => 0xe04b,
        "right" => 0xe04d,
        "end" => 0xe04f,
        "down" => 0xe050,
        "pagedown" => 0xe051,
        "insert" => 0xe052,
        "delete" => 0xe053,
        "printscreen" => 0xe037,
        "super" | "cmd" => 0xe05b,
        "menu" => 0xe05d,
        _ => return None,
    })
}

fn is_x11() -> bool {
    #[cfg(target_os = "linux")]
    {
        gpui::guess_compositor() == "X11"
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_axes_keep_windows_direction_and_one_detent() {
        let mut remainder = (0., 0.);
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(0., 3.)), &mut remainder),
            [(false, 120), (true, 0)]
        );
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(-3., 0.)), &mut remainder),
            [(false, 0), (true, 120)]
        );
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(3., -3.)), &mut remainder),
            [(false, -120), (true, -120)]
        );
    }

    #[test]
    fn wheel_fractions_stay_on_their_axis_and_large_motion_is_not_clipped_to_wire_size() {
        let mut remainder = (0., 0.);
        assert_eq!(
            wheel_deltas(
                ScrollDelta::Pixels(point(px(-0.25), px(0.))),
                &mut remainder
            ),
            [(false, 0), (true, 0)]
        );
        assert_eq!(
            wheel_deltas(ScrollDelta::Pixels(point(px(0.), px(0.25))), &mut remainder),
            [(false, 0), (true, 0)]
        );
        assert_eq!(
            wheel_deltas(
                ScrollDelta::Pixels(point(px(-0.25), px(0.))),
                &mut remainder
            ),
            [(false, 0), (true, 1)]
        );
        assert_eq!(remainder, (0.5, 0.75));
        assert_eq!(
            wheel_deltas(ScrollDelta::Lines(point(30., -30.)), &mut remainder),
            [(false, -1199), (true, -1199)]
        );
        assert_eq!(remainder, (-0.5, -0.25));
    }

    #[test]
    fn local_pan_shortcut_chooses_one_axis_after_platform_shift_mapping() {
        let shifted = ScrollDelta::Lines(point(-3., 0.));
        assert_eq!(local_pan_delta(shifted, false), (0., -120.));
        assert_eq!(local_pan_delta(shifted, true), (-120., 0.));
        let diagonal = ScrollDelta::Pixels(point(px(10.), px(-10.)));
        assert_eq!(local_pan_delta(diagonal, false), (0., -10.));
        assert_eq!(local_pan_delta(diagonal, true), (-10., 0.));
    }

    #[test]
    fn modifier_only_events_forward_presses_and_releases_once() {
        for (modifiers, scancode) in [
            (
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                0x38,
            ),
            (
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                0x2a,
            ),
            (
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                0x1d,
            ),
            (
                Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                0xe05b,
            ),
        ] {
            let mut remote_keys = BTreeSet::new();
            let press = modifier_changes(Modifiers::default(), modifiers, &remote_keys, true);
            assert_eq!(
                press.into_iter().flatten().collect::<Vec<_>>(),
                [(scancode, true)]
            );
            remote_keys.insert(scancode);
            assert!(
                modifier_changes(modifiers, modifiers, &remote_keys, true)
                    .into_iter()
                    .flatten()
                    .next()
                    .is_none()
            );
            let release = modifier_changes(modifiers, Modifiers::default(), &remote_keys, true);
            assert_eq!(
                release.into_iter().flatten().collect::<Vec<_>>(),
                [(scancode, false)]
            );
        }
    }

    #[test]
    fn local_shortcut_release_does_not_repress_a_remaining_modifier() {
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let chord = Modifiers {
            shift: true,
            ..control
        };
        // Ctrl+Shift+Enter has released all remote keys for the local zoom action.
        let remote_keys = BTreeSet::new();
        for (before, after) in [(chord, control), (control, Modifiers::default())] {
            assert!(
                modifier_changes(before, after, &remote_keys, true)
                    .into_iter()
                    .flatten()
                    .next()
                    .is_none()
            );
        }
        assert_eq!(
            modifier_changes(Modifiers::default(), control, &remote_keys, true)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            [(0x1d, true)],
        );
    }

    #[test]
    fn disabled_modifier_input_releases_keys_without_delaying_new_presses() {
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let remote_keys = BTreeSet::from([0x1d]);
        // An inactive window or local IME composition may release Ctrl, but a
        // new Alt press is neither forwarded nor deferred until input resumes.
        assert_eq!(
            modifier_changes(control, alt, &remote_keys, false)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            [(0x1d, false)],
        );
        assert!(
            modifier_changes(alt, alt, &BTreeSet::new(), true)
                .into_iter()
                .flatten()
                .next()
                .is_none()
        );
    }

    #[test]
    fn ime_ranges_preserve_utf8_and_surrogate_pairs() {
        let text = "中😀e\u{301}";
        assert_eq!(&text[utf16_range(text, 1..3)], "😀");
        assert_eq!(&text[utf16_range(text, 2..3)], "");
        assert_eq!(&text[utf16_range(text, 0..99)], text);
    }

    #[test]
    fn english_typing_reaches_remote_ime_as_keys_including_candidate_selection() {
        let plain = Modifiers::default();
        for (key, text, code) in [
            ("n", Some("n"), 0x31),
            ("i", Some("i"), 0x17),
            ("h", Some("h"), 0x23),
            ("a", Some("a"), 0x1e),
            ("o", Some("o"), 0x18),
            ("space", Some(" "), 0x39),
            ("1", Some("1"), 0x02),
            ("'", Some("'"), 0x28),
            ("enter", None, 0x1c),
            ("backspace", None, 0x0e),
            ("delete", None, 0xe053),
            ("f12", None, 0x58),
        ] {
            let key = Keystroke {
                key: key.into(),
                key_char: text.map(str::to_owned),
                modifiers: plain,
            };
            assert_eq!(key_scancode(&key, plain), Some((code, plain)));
        }
    }

    #[test]
    fn shifted_symbols_preserve_shift_and_release_the_same_key_after_shift_is_released() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        for (shifted, plain) in [
            ("!", "1"),
            ("@", "2"),
            ("#", "3"),
            ("$", "4"),
            ("%", "5"),
            ("^", "6"),
            ("&", "7"),
            ("*", "8"),
            ("(", "9"),
            (")", "0"),
            ("_", "-"),
            ("+", "="),
            ("{", "["),
            ("}", "]"),
            (":", ";"),
            ("\"", "'"),
            ("~", "`"),
            ("|", "\\"),
            ("<", ","),
            (">", "."),
            ("?", "/"),
        ] {
            // GPUI's symbol event omits Shift; Window still knows it is held.
            let key = Keystroke {
                key: shifted.into(),
                key_char: Some(shifted.into()),
                modifiers: Modifiers::default(),
            };
            let (code, modifiers) = key_scancode(&key, shift).unwrap();
            assert_eq!(modifiers, shift);
            let mut held = BTreeSet::from([code]);
            assert!(held.remove(&scancode(plain).unwrap()));
            assert_eq!(scancode(shifted), Some(code));
        }
    }

    #[test]
    fn local_layout_text_and_altgr_keep_unicode_delivery() {
        let plain = Modifiers::default();
        for (key, text) in [
            ("n", "你"),
            ("q", "ๆ"),
            ("eacute", "é"),
            ("a", "A"),
            ("a", "abc"),
            ("!", "!"),
        ] {
            let key = Keystroke {
                key: key.into(),
                key_char: Some(text.into()),
                modifiers: plain,
            };
            assert_eq!(key_scancode(&key, plain), None);
        }
        let alt_gr = Modifiers {
            control: true,
            alt: true,
            ..plain
        };
        let key = Keystroke {
            key: "q".into(),
            key_char: Some("@".into()),
            modifiers: alt_gr,
        };
        assert_eq!(key_scancode(&key, alt_gr), None);
        assert_eq!(alt_gr_text(&key), Some("@"));

        let shift = Modifiers {
            shift: true,
            ..plain
        };
        // Caps Lock+Shift and layouts with shifted digits must retain their
        // committed text rather than becoming uppercase letters or US symbols.
        for key in ["a", "1"] {
            let key = Keystroke {
                key: key.into(),
                key_char: Some(key.into()),
                modifiers: shift,
            };
            assert_eq!(key_scancode(&key, shift), None);
        }
    }

    #[test]
    fn shifted_letters_and_shortcuts_keep_remote_modifiers() {
        for (modifiers, text) in [
            (
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Some("A"),
            ),
            (
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                None,
            ),
        ] {
            let key = Keystroke {
                key: "a".into(),
                key_char: text.map(str::to_owned),
                modifiers,
            };
            assert_eq!(key_scancode(&key, modifiers), Some((0x1e, modifiers)));
        }
    }

    #[test]
    fn clipboard_shortcuts_respect_remote_modifiers_and_sharing_setting() {
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let ctrl_shift = Modifiers {
            shift: true,
            ..ctrl
        };
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(paste_shortcut("v", ctrl, true), Some(false));
        assert_eq!(paste_shortcut("v", ctrl_shift, true), Some(true));
        assert_eq!(paste_shortcut("insert", shift, true), Some(false));
        assert_eq!(paste_shortcut("v", ctrl, false), None);
        assert_eq!(paste_shortcut("v", ctrl_shift, false), Some(true));
        assert_eq!(paste_shortcut("insert", shift, false), None);
        for mods in [ctrl, ctrl_shift] {
            assert_eq!(
                paste_shortcut("v", Modifiers { alt: true, ..mods }, true),
                None
            );
            assert_eq!(
                paste_shortcut(
                    "v",
                    Modifiers {
                        platform: true,
                        ..mods
                    },
                    true
                ),
                None
            );
        }
        assert_eq!(paste_shortcut("c", ctrl, true), None);
        assert_eq!(paste_shortcut("v", Modifiers::default(), true), None);
    }

    #[test]
    fn local_clipboard_rejects_unsupported_text_before_retaining_a_snapshot() {
        assert_eq!(clipboard_text(None), Ok(None));
        assert_eq!(
            clipboard_text(Some(ClipboardItem::new_string("中文😀\nline two".into()))),
            Ok(Some("中文😀\nline two".into())),
        );
        assert_eq!(
            clipboard_text(Some(ClipboardItem::new_string("a\0b".into()))),
            Err(i18n_key::RDP_CLIPBOARD_TEXT_CANNOT_CONTAIN_NUL_CHARACTERS),
        );
        assert_eq!(
            clipboard_text(Some(ClipboardItem::new_string(
                "x".repeat(rustterm::rdp::MAX_CLIPBOARD_BYTES + 1)
            ))),
            Err(i18n_key::RDP_CLIPBOARD_TEXT_IS_LIMITED_TO_1_MIB),
        );
    }
}
