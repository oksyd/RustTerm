use super::{PADDING, TerminalView};
use crate::app::{layout::Axis, pane::PaneEvent, text_input::alt_gr_text};
use gpui::{
    Bounds, Context, EntityInputHandler, KeyDownEvent, Modifiers, Pixels, Point, ScrollDelta,
    ScrollWheelEvent, UTF16Selection, Window, px,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    session::{Command, SessionState},
    terminal::SelectionKind,
};
use std::ops::Range;

pub(super) struct SelectionDrag {
    origin: (u16, u16),
    publish: bool,
    last_update: Option<(u16, u16, u64)>,
}

impl SelectionDrag {
    pub(super) fn scrolled(&mut self) {
        self.publish = true;
        self.last_update = None;
    }

    fn update(&mut self, column: u16, row: u16, version: u64) -> bool {
        self.publish |= (column, row) != self.origin;
        let update = (column, row, version);
        if self.last_update == Some(update) {
            return false;
        }
        self.last_update = Some(update);
        true
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum WheelTarget {
    #[default]
    History,
    Mouse,
    Alternate,
}

#[derive(Default)]
pub(super) struct WheelState {
    remainder: Point<f32>,
    target: WheelTarget,
}

impl WheelState {
    fn steps(
        &mut self,
        delta: ScrollDelta,
        cell: Point<Pixels>,
        target: WheelTarget,
    ) -> Point<i32> {
        if self.target != target {
            self.remainder = Point::default();
            self.target = target;
        }
        let delta = match delta {
            ScrollDelta::Lines(delta) => delta,
            ScrollDelta::Pixels(delta) => Point {
                x: f32::from(delta.x) / f32::from(cell.x),
                y: f32::from(delta.y) / f32::from(cell.y),
            },
        };
        let take = |remainder: &mut f32, delta: f32| {
            let total = (*remainder + delta).clamp(-100., 100.);
            let steps = total.trunc() as i32;
            *remainder = total - steps as f32;
            steps
        };
        Point {
            x: if target == WheelTarget::Mouse {
                take(&mut self.remainder.x, delta.x)
            } else {
                // Local history has no horizontal axis.
                self.remainder.x = 0.;
                0
            },
            y: take(&mut self.remainder.y, delta.y),
        }
    }
}

impl TerminalView {
    pub(super) fn search_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_open
            || self.menu_open
            || !self.visible
            || !self.window_state.visibility.is_visible()
            || !window.is_window_active()
            || self.chrome_blocked(cx)
            || !self.focus.contains_focused(window, cx)
        {
            return;
        }
        let input_focused = self.search_input.read(cx).focus_handle().is_focused(window);
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        match key.key.as_str() {
            "escape" if !modifiers.shift => {
                self.close_search(cx);
                self.focus.focus(window);
                cx.notify();
            }
            "f3" => self.send(
                Command::NextMatch {
                    backwards: modifiers.shift,
                },
                cx,
            ),
            "enter" | "return" if input_focused && modifiers.shift => {
                self.send(Command::NextMatch { backwards: true }, cx);
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.chrome_blocked(cx) {
            return;
        }
        self.send(Command::Paste(text.to_owned()), cx);
    }

    pub(super) fn release_input(&mut self) {
        self.copy_task.take();
        self.primary_copy_task.take();
        self.wheel = WheelState::default();
        self.preedit.clear();
        self.preedit_selection = 0..0;
        self.selection_drag = None;
        self.selection_pointer = None;
        if let Some(button) = self.mouse_button.take()
            && let Some(run) = self.runs.get(self.viewed_run)
        {
            let _ = run.send(Command::Mouse {
                button,
                column: self.last_mouse.0,
                row: self.last_mouse.1,
                pressed: false,
                motion: false,
                shift: false,
                alt: false,
                ctrl: false,
            });
        }
    }

    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu_key(event, window, cx) {
            return;
        }
        if !self.visible
            || !window.is_window_active()
            || !self.focus.is_focused(window)
            || self.window_state.prompts.is_busy()
            || self.window_state.prompts.is_blocked()
        {
            return;
        }
        if !self.preedit.is_empty() || self.paste_pending {
            return;
        }
        let key = &event.keystroke;
        let mods = key.modifiers;
        if mods.platform {
            return;
        }
        // GPUI's Linux backends only commit text automatically when the modifiers
        // are a subset of Shift. AltGr text must bypass local Ctrl shortcuts.
        if let Some(text) = alt_gr_text(key) {
            self.commit_text(text, window, cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if let Some(bytes) = meta_input(key) {
            self.send(Command::Input(bytes), cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if !mods.control
            && !mods.alt
            && ((mods.shift && key.key == "f10") || (!mods.shift && key.key == "menu"))
        {
            if let Some(bounds) = self.cursor_bounds().or(self.bounds) {
                self.open_context_menu(bounds.origin, window, cx);
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if !mods.alt && key.key == "insert" {
            if mods.control && !mods.shift {
                self.copy_selection(cx);
            } else if mods.shift && !mods.control {
                self.paste(window, cx);
            } else {
                self.send(
                    Command::Key {
                        key: key.key.clone(),
                        ctrl: mods.control,
                        alt: false,
                        shift: mods.shift,
                    },
                    cx,
                );
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if mods.shift
            && !mods.control
            && !mods.alt
            && let Some(snapshot) = &self.snapshot
            && !snapshot.alternate_screen
        {
            let page = i32::from(snapshot.rows.saturating_sub(1).max(1));
            let command = match key.key.as_str() {
                "pageup" | "page-up" => Some(Command::Scroll(page)),
                "pagedown" | "page-down" => Some(Command::Scroll(-page)),
                "home" => Some(Command::ScrollTo(usize::MAX)),
                "end" => Some(Command::ScrollToBottom),
                _ => None,
            };
            if let Some(command) = command {
                self.send(command, cx);
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
        }
        if mods.control && mods.shift && !mods.alt {
            match key.key.to_ascii_lowercase().as_str() {
                "up" | "down"
                    if !self.show_final_alternate
                        && self.snapshot.as_ref().is_some_and(|snapshot| {
                            !snapshot.alternate_screen && !snapshot.shell.commands.is_empty()
                        }) =>
                {
                    self.navigate_command(key.key == "up", false, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                "b" => return, // The workspace owns the sidebar shortcut.
                "enter" | "return" => {
                    cx.emit(PaneEvent::Zoom);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                "a" => {
                    self.send(Command::SelectAll, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                "f" => {
                    self.open_search(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "s" => {
                    cx.emit(PaneEvent::Files);
                    cx.stop_propagation();
                    return;
                }
                "t" => {
                    cx.emit(PaneEvent::NewTab);
                    cx.stop_propagation();
                    return;
                }
                "w" => {
                    cx.emit(PaneEvent::Close(self.index));
                    cx.stop_propagation();
                    return;
                }
                "d" => {
                    cx.emit(PaneEvent::Split(Axis::Horizontal));
                    cx.stop_propagation();
                    return;
                }
                "e" => {
                    cx.emit(PaneEvent::Split(Axis::Vertical));
                    cx.stop_propagation();
                    return;
                }
                "j" => {
                    cx.emit(PaneEvent::NextPane);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if mods.control && !mods.alt && key.key == "tab" {
            // The workspace handles both directions, including Ctrl+Shift+Tab.
            return;
        }
        if mods.control && mods.shift && !mods.alt && key.key.eq_ignore_ascii_case("v") {
            self.paste(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.alt && matches!(key.key.as_str(), "+" | "=" | "-" | "0") {
            self.font_size = match key.key.as_str() {
                "-" => (self.font_size - 1.).max(8.),
                "0" => self.initial_font_size,
                _ => (self.font_size + 1.).min(40.),
            };
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let special = matches!(
            key.key.as_str(),
            "enter"
                | "return"
                | "backspace"
                | "delete"
                | "escape"
                | "tab"
                | "up"
                | "down"
                | "left"
                | "right"
                | "home"
                | "end"
                | "pageup"
                | "page-up"
                | "pagedown"
                | "page-down"
                | "insert"
        ) || (key.key.starts_with('f')
            && key.key[1..]
                .parse::<u8>()
                .is_ok_and(|n| (1..=12).contains(&n)));
        if special || mods.control || mods.alt {
            self.send(
                Command::Key {
                    key: key.key.clone(),
                    ctrl: mods.control,
                    alt: mods.alt,
                    shift: mods.shift,
                },
                cx,
            );
            cx.stop_propagation();
        }
        // Ordinary text (including IME commits) goes exclusively through EntityInputHandler.
    }

    pub(super) fn accepts_text_input(&self, window: &Window) -> bool {
        self.visible
            && self.window_state.visibility.is_visible()
            && window.is_window_active()
            && self.focus.is_focused(window)
            && !self.paste_pending
            && !self.window_state.prompts.is_busy()
            && !self.window_state.prompts.is_blocked()
            && self.can_send_input()
    }

    pub(super) fn can_send_input(&self) -> bool {
        !self.show_final_alternate
            && self.viewed_run + 1 == self.runs.len()
            && self
                .runs
                .get(self.viewed_run)
                .is_some_and(|run| run.state() == SessionState::Connected && !run.is_read_only())
    }

    fn commit_text(&mut self, text: &str, window: &Window, cx: &mut Context<Self>) {
        if self.accepts_text_input(window) && !text.is_empty() {
            self.send(Command::Input(text.as_bytes().to_vec()), cx);
        }
    }

    pub(super) fn begin_selection(
        &mut self,
        position: Point<Pixels>,
        kind: SelectionKind,
        cx: &mut Context<Self>,
    ) {
        if self.chrome_blocked(cx) || self.show_final_alternate {
            return;
        }
        let Some((column, row)) = self.mouse_position(position) else {
            return;
        };
        self.primary_copy_task.take();
        self.selection_drag = Some(SelectionDrag {
            origin: (column, row),
            publish: kind != SelectionKind::Character,
            last_update: None,
        });
        self.selection_pointer = Some(position);
        self.send(Command::BeginSelection { column, row, kind }, cx);
    }

    pub fn drag_selection(
        &mut self,
        position: Point<Pixels>,
        released: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        if released {
            // Retry the final extent even if an earlier move hit a full queue.
            drag.last_update = None;
        }
        self.selection_pointer = Some(position);
        self.extend_selection(position, cx);
        if released && let Some(drag) = self.selection_drag.take() {
            self.selection_pointer = None;
            if drag.publish {
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                self.copy_primary(cx);
            } else {
                self.send(Command::ClearSelection, cx);
            }
        }
    }

    pub(super) fn extend_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let column = ((f32::from(position.x - bounds.left() - px(PADDING) - self.gutter_width())
            / f32::from(self.cell_width))
        .floor()
        .max(0.) as u16)
            .min(snapshot.columns - 1);
        let row = ((f32::from(position.y - bounds.top() - px(PADDING))
            / f32::from(self.line_height))
        .floor()
        .max(0.) as u16)
            .min(snapshot.rows - 1);
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        if !drag.update(column, row, snapshot.version) {
            return;
        }
        self.send(Command::UpdateSelection { column, row }, cx);
    }

    pub(super) fn mouse_position(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.bounds?;
        let x: f32 = (position.x - bounds.left() - px(PADDING) - self.gutter_width()).into();
        let y: f32 = (position.y - bounds.top() - px(PADDING)).into();
        let width: f32 = self.cell_width.into();
        let height: f32 = self.line_height.into();
        let snapshot = self.snapshot.as_ref()?;
        if x < 0. || y < 0. {
            return None;
        }
        let (column, row) = ((x / width) as u16, (y / height) as u16);
        (column < snapshot.columns && row < snapshot.rows).then_some((column, row))
    }

    pub(super) fn mouse(
        &mut self,
        position: Point<Pixels>,
        button: u8,
        pressed: bool,
        motion: bool,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let releasing_capture = !pressed && !motion && self.mouse_button == Some(button);
        if !pressed && !motion && button < 3 && !releasing_capture {
            return;
        }
        if !pressed && !motion && !releasing_capture {
            return;
        }
        if (modifiers.shift && !releasing_capture)
            || !self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.modes.mouse_click || s.modes.mouse_drag || s.modes.mouse_motion)
        {
            return;
        }
        let coordinates = self
            .mouse_position(position)
            .or_else(|| releasing_capture.then_some(self.last_mouse));
        let Some((column, row)) = coordinates else {
            return;
        };
        if motion && self.last_mouse == (column, row) {
            return;
        }
        self.last_mouse = (column, row);
        if !motion && button < 3 {
            self.mouse_button = pressed.then_some(button);
        }
        self.send(
            Command::Mouse {
                button,
                column,
                row,
                pressed,
                motion,
                shift: modifiers.shift && !releasing_capture,
                alt: modifiers.alt,
                ctrl: modifiers.control,
            },
            cx,
        );
    }

    pub(super) fn scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Even sub-cell deltas belong to this canvas; a parent must not consume
        // the same gesture while it is being accumulated here.
        cx.stop_propagation();
        let reporting = !event.modifiers.shift
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.modes.mouse_click || s.modes.mouse_drag || s.modes.mouse_motion);
        let target = if reporting {
            WheelTarget::Mouse
        } else if let Some(snapshot) = &self.snapshot
            && snapshot.alternate_screen
        {
            if event.modifiers.shift || !snapshot.modes.alternate_scroll || snapshot.is_read_only {
                self.wheel = WheelState::default();
                return;
            }
            WheelTarget::Alternate
        } else {
            WheelTarget::History
        };
        let steps = self.wheel.steps(
            event.delta,
            Point {
                x: self.cell_width,
                y: self.line_height,
            },
            target,
        );
        if reporting {
            for (steps, positive, negative) in [(steps.y, 64, 65), (steps.x, 66, 67)] {
                for _ in 0..steps.unsigned_abs() {
                    self.mouse(
                        event.position,
                        if steps > 0 { positive } else { negative },
                        true,
                        false,
                        event.modifiers,
                        cx,
                    );
                }
            }
        } else if steps.y != 0 {
            self.send(
                match target {
                    WheelTarget::Alternate => Command::AlternateScroll(steps.y),
                    _ => Command::Scroll(steps.y),
                },
                cx,
            );
        }
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = utf16_range(&self.preedit, range);
        *actual = Some(
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
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.preedit.clear();
        self.preedit_selection = 0..0;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit.clear();
        self.preedit_selection = 0..0;
        self.commit_text(text, window, cx);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_text_input(window) {
            return;
        }
        if self.snapshot.as_ref().is_some_and(|s| s.display_offset > 0) {
            self.send(Command::ScrollToBottom, cx);
        }
        let bytes = range
            .map(|r| utf16_range(&self.preedit, r))
            .unwrap_or(0..self.preedit.len());
        if self.preedit.len() - bytes.len() + text.len() > 8192
            || text.chars().any(char::is_control)
        {
            self.error = Some(
                i18n_key::TERMINAL_IME_COMPOSITION_WAS_NOT_SENT_TOO_LONG_OR_CONTAINS_CONTROL.into(),
            );
            cx.notify();
            return;
        }
        let start = self.preedit[..bytes.start].encode_utf16().count();
        self.preedit.replace_range(bytes, text);
        let inserted = text.encode_utf16().count();
        self.preedit_selection = selection
            .map(|r| {
                start + r.start.min(inserted)
                    ..start + r.end.min(inserted).max(r.start.min(inserted))
            })
            .unwrap_or(start + inserted..start + inserted);
        cx.notify(); // Preedit is visual-only; no session command is sent.
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds()
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

fn meta_input(key: &gpui::Keystroke) -> Option<Vec<u8>> {
    if !key.modifiers.alt || key.modifiers.control || key.modifiers.platform {
        return None;
    }
    let text = key.key_char.as_deref()?;
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    // The normalized shortcut key loses layout, Caps Lock and keypad text.
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.push(0x1b);
    bytes.extend_from_slice(text.as_bytes());
    Some(bytes)
}

fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    fn offset(text: &str, target: usize) -> usize {
        let mut count = 0;
        for (byte, ch) in text.char_indices() {
            if count >= target {
                return byte;
            }
            count += ch.len_utf16();
        }
        text.len()
    }
    let start = offset(text, range.start);
    start..offset(text, range.end).max(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_drag_coalesces_cell_motion_without_losing_scroll_or_output_updates() {
        let mut drag = SelectionDrag {
            origin: (2, 3),
            publish: false,
            last_update: None,
        };
        assert!(drag.update(2, 3, 1));
        assert!(!drag.update(2, 3, 1));
        assert!(!drag.publish);
        assert!(drag.update(4, 3, 1));
        assert!(drag.publish);
        assert!(!drag.update(4, 3, 1));
        assert!(drag.update(4, 3, 2));
        drag.scrolled();
        assert!(drag.update(4, 3, 2));
        assert!(!drag.update(4, 3, 2));
    }

    #[test]
    fn meta_input_preserves_layout_caps_lock_and_keypad_text() {
        let mut key = gpui::Keystroke {
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            key: "c".into(),
            key_char: Some("C".into()),
        };
        assert_eq!(meta_input(&key).as_deref(), Some(b"\x1bC".as_slice()));
        key.key_char = Some("ç".into());
        assert_eq!(meta_input(&key).as_deref(), Some("\x1bç".as_bytes()));
        key.key = "decimal".into();
        key.key_char = Some(",".into());
        assert_eq!(meta_input(&key).as_deref(), Some(b"\x1b,".as_slice()));
        key.key = "left".into();
        key.key_char = None;
        assert!(meta_input(&key).is_none());
        key.key_char = Some("\r".into());
        assert!(meta_input(&key).is_none());
        key.key_char = Some("@".into());
        key.modifiers.control = true;
        assert!(meta_input(&key).is_none());
        key.modifiers.control = false;
        key.modifiers.platform = true;
        assert!(meta_input(&key).is_none());
        key.modifiers.platform = false;
        key.modifiers.alt = false;
        assert!(meta_input(&key).is_none());
    }

    #[test]
    fn wheel_keeps_fractional_axes_independent() {
        let mut wheel = WheelState::default();
        let cell = Point {
            x: px(8.),
            y: px(16.),
        };
        let delta = ScrollDelta::Pixels(Point {
            x: px(4.),
            y: px(-4.),
        });
        assert_eq!(
            wheel.steps(delta, cell, WheelTarget::Mouse),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(delta, cell, WheelTarget::Mouse),
            Point { x: 1, y: 0 }
        );
        assert_eq!(
            wheel.steps(delta, cell, WheelTarget::Mouse),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(delta, cell, WheelTarget::Mouse),
            Point { x: 1, y: -1 }
        );
    }

    #[test]
    fn local_history_never_turns_horizontal_input_into_vertical_scroll() {
        let mut wheel = WheelState::default();
        let cell = Point {
            x: px(8.),
            y: px(16.),
        };
        let horizontal = ScrollDelta::Lines(Point { x: 2., y: 0. });
        assert_eq!(
            wheel.steps(horizontal, cell, WheelTarget::History),
            Point { x: 0, y: 0 }
        );
        let fractional = ScrollDelta::Lines(Point { x: 0.75, y: 0.75 });
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::History),
            Point { x: 0, y: 0 }
        );
        // Switching between mouse reporting and local history starts a fresh
        // accumulator instead of forwarding a prior mode's partial gesture.
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::Mouse),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::Mouse),
            Point { x: 1, y: 1 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::History),
            Point { x: 0, y: 0 }
        );
    }

    #[test]
    fn alternate_scroll_does_not_reuse_history_or_mouse_gestures() {
        let mut wheel = WheelState::default();
        let cell = Point {
            x: px(8.),
            y: px(16.),
        };
        let fractional = ScrollDelta::Lines(Point { x: 0.75, y: 0.75 });
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::History),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::Alternate),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::Alternate),
            Point { x: 0, y: 1 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::Mouse),
            Point { x: 0, y: 0 }
        );
        assert_eq!(
            wheel.steps(fractional, cell, WheelTarget::History),
            Point { x: 0, y: 0 }
        );
    }

    #[test]
    fn alt_gr_preserves_the_text_payload_instead_of_the_shortcut_key() {
        let mut key = gpui::Keystroke {
            modifiers: Modifiers {
                control: true,
                alt: true,
                shift: true,
                ..Modifiers::default()
            },
            key: "t".into(),
            key_char: Some("Þ".into()),
        };
        // The normalized shortcut key can name a local action, while the text
        // payload is the actual character produced by the keyboard layout.
        assert_eq!(alt_gr_text(&key), Some("Þ"));
        key.key = "left".into();
        key.key_char = None;
        assert_eq!(alt_gr_text(&key), None); // Keep Ctrl+Alt+Left as protocol input.
        key.key = "a".into();
        key.key_char = Some("A".into());
        key.modifiers.control = false;
        key.modifiers.alt = false;
        assert_eq!(alt_gr_text(&key), None); // GPUI commits ordinary text once.
        key.modifiers.control = true;
        key.modifiers.alt = true;
        key.modifiers.platform = true;
        assert_eq!(alt_gr_text(&key), None); // Desktop shortcuts remain local.
    }

    #[test]
    fn ime_utf16_ranges_never_split_utf8_or_surrogates() {
        let text = "中😀e\u{301}";
        assert_eq!(&text[utf16_range(text, 1..3)], "😀");
        assert_eq!(&text[utf16_range(text, 2..3)], "");
        assert_eq!(&text[utf16_range(text, 0..99)], text);
        assert_eq!(&text[utf16_range(text, Range { start: 5, end: 2 })], "");
    }
}
