use super::{PADDING, TerminalView};
use crate::app::{layout::Axis, pane::PaneEvent, text_input::alt_gr_text};
use gpui::{
    Bounds, Context, DispatchPhase, EntityInputHandler, KeyDownEvent, Modifiers, MouseButton,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, UTF16Selection,
    Window, px,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    session::{Command, SessionState},
    terminal::{SelectionKind, SelectionPoint, SelectionSide},
};
use std::ops::Range;

pub(super) fn local_selection_kind(
    clicks: usize,
    modifiers: Modifiers,
    mouse_reporting: bool,
    archived: bool,
) -> Option<SelectionKind> {
    let enclosed = modifiers.alt && !modifiers.control && !modifiers.platform;
    // Reserve the first click too: a double-click must never send half a gesture
    // to an application that enabled terminal mouse reporting.
    if mouse_reporting && !modifiers.shift && !archived && !enclosed {
        return None;
    }
    Some(match clicks {
        2 if enclosed => SelectionKind::Enclosed,
        2 => SelectionKind::Word,
        3.. => SelectionKind::Line,
        _ => SelectionKind::Character,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    Scroll(f64),
    Selection(SelectionPoint),
}

#[derive(Default)]
pub(super) struct FrameMotion {
    pending: Option<Motion>,
    scheduled: bool,
}

impl FrameMotion {
    pub(super) fn discard(&mut self) {
        self.pending = None;
    }

    fn push(&mut self, next: Motion) -> Option<Motion> {
        match (self.pending, next) {
            (Some(Motion::Scroll(previous)), Motion::Scroll(delta))
                if previous.signum() == delta.signum() =>
            {
                self.pending = Some(Motion::Scroll(previous + delta));
                None
            }
            (Some(Motion::Selection(_)), Motion::Selection(_)) => {
                self.pending = Some(next);
                None
            }
            _ => self.pending.replace(next),
        }
    }
}

pub(super) struct SelectionDrag {
    origin: SelectionPoint,
    pointer_origin: Point<Pixels>,
    publish: bool,
    last_update: Option<(SelectionPoint, u64)>,
    deferred: bool,
    pub(super) scroll: DragScroll,
}

impl SelectionDrag {
    fn new(
        origin: SelectionPoint,
        position: Point<Pixels>,
        kind: SelectionKind,
        intent: SelectionIntent,
    ) -> Self {
        Self {
            origin,
            pointer_origin: position,
            publish: kind != SelectionKind::Character || intent == SelectionIntent::Extend,
            deferred: intent == SelectionIntent::Preserve,
            last_update: None,
            scroll: DragScroll::default(),
        }
    }

    pub(super) fn scrolled(&mut self) {
        self.publish = true;
        self.last_update = None;
    }

    pub(super) fn scroll_distance(
        &self,
        pointer: Point<Pixels>,
        bounds: Bounds<Pixels>,
        line_height: Pixels,
    ) -> f32 {
        // Start within two rows of the edge: maximized windows cannot rely on
        // reaching a position outside the screen. Reserve the middle half of
        // even a small pane for selection without scrolling.
        let band = (f32::from(line_height).max(1.) * 2.)
            .min(f32::from(bounds.size.height) / 4.)
            .max(1.);
        let movement = f32::from(pointer.y - self.pointer_origin.y);
        if movement < -2. && pointer.y < bounds.top() + px(band) {
            f32::from(bounds.top() + px(band) - pointer.y) / band * 6.
        } else if movement > 2. && pointer.y > bounds.bottom() - px(band) {
            f32::from(bounds.bottom() - px(band) - pointer.y) / band * 6.
        } else {
            0.
        }
    }

    fn update(&mut self, position: Point<Pixels>, point: SelectionPoint, version: u64) -> bool {
        // Match GPUI's two logical pixel drag threshold. Crossing a cell edge
        // with a tiny click jitter must not turn a focus click into a selection.
        self.publish |= (self.deferred || point != self.origin)
            && (position - self.pointer_origin).magnitude() > 2.;
        let update = (point, version);
        if !self.publish || self.last_update == Some(update) {
            return false;
        }
        self.last_update = Some(update);
        true
    }

    fn finish(&self) -> SelectionFinish {
        if self.deferred {
            SelectionFinish::Keep
        } else if self.publish {
            SelectionFinish::Copy
        } else {
            SelectionFinish::Clear
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SelectionFinish {
    Keep,
    Clear,
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionIntent {
    New,
    Extend,
    Preserve,
}

pub(super) fn selection_intent(
    focused: bool,
    selected: bool,
    clicks: usize,
    modifiers: Modifiers,
) -> SelectionIntent {
    if clicks == 1 && !modifiers.control && !modifiers.alt && !modifiers.platform {
        if modifiers.shift {
            return SelectionIntent::Extend;
        }
        if !focused && selected {
            return SelectionIntent::Preserve;
        }
    }
    SelectionIntent::New
}

fn selection_hit(
    snapshot: &rustterm::terminal::Snapshot,
    x: f32,
    y: f32,
    cell_width: f32,
    line_height: f32,
) -> Option<SelectionPoint> {
    let row = (y / line_height.max(1.) + snapshot.scroll_fraction)
        .floor()
        .max(0.) as usize;
    let row = row.min(snapshot.lines.len().checked_sub(1)?);
    let cells = &snapshot.lines.get(row)?.cells;
    let x = x / cell_width.max(1.);
    let column = (x.floor().max(0.) as usize).min(cells.len().checked_sub(1)?);
    let mut lead = column;
    if cells.get(lead)?.width == 0 && lead > 0 {
        lead -= 1;
        if cells[lead].width != 2 {
            return Some(SelectionPoint::new(
                lead as u16,
                row as u16,
                SelectionSide::Right,
            ));
        }
    }
    let width = usize::from(cells.get(lead)?.width.max(1));
    let right = x >= lead as f32 + width as f32 / 2.;
    let column = if right {
        (lead + width - 1).min(cells.len() - 1)
    } else {
        lead
    };
    Some(SelectionPoint::new(
        column as u16,
        row as u16,
        if right {
            SelectionSide::Right
        } else {
            SelectionSide::Left
        },
    ))
}

#[derive(Default)]
pub(super) struct DragScroll {
    last: Option<std::time::Instant>,
    direction: i32,
    remainder: f32,
}

impl DragScroll {
    /// Distance beyond the viewport in line heights; positive scrolls upward.
    pub(super) fn step(&mut self, distance: f32, now: std::time::Instant) -> i32 {
        let direction = if distance > 0. {
            1
        } else if distance < 0. {
            -1
        } else {
            0
        };
        let previous = self.last.replace(now);
        if direction == 0 || direction != self.direction {
            self.direction = direction;
            self.remainder = 0.;
            return 0;
        }
        let Some(previous) = previous else {
            return 0;
        };
        // Fractional lines make speed independent of frame rate. Limit catch-up
        // after a stalled frame so a short drag cannot skip a screenful.
        let seconds = now
            .saturating_duration_since(previous)
            .as_secs_f32()
            .min(0.1);
        let speed = (4. + 2. * distance * distance).min(120.);
        self.remainder += seconds * speed;
        let lines = self.remainder.floor() as i32;
        self.remainder -= lines as f32;
        direction * lines
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum WheelTarget {
    #[default]
    History,
    Mouse,
    Alternate,
    Font,
}

#[derive(Default)]
pub(super) struct WheelState {
    remainder: Point<f32>,
    target: WheelTarget,
}

impl WheelState {
    fn zoom(&mut self, delta: ScrollDelta, font_size: f32) -> f32 {
        // Keep touchpad sensitivity independent of the current font metrics.
        // A mouse wheel event can contain several lines; change one point at
        // a time instead of multiplying by the system's scroll-line setting.
        let steps = self.steps(
            delta,
            Point {
                x: px(40.),
                y: px(40.),
            },
            WheelTarget::Font,
        );
        (font_size + steps.y.signum() as f32).clamp(8., 40.)
    }

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
        if input_focused {
            self.search_seed_task.take();
        }
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

    pub fn send_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.chrome_blocked(cx) {
            return;
        }
        self.paste_text(text.to_owned(), window, cx);
    }

    fn deliver_motion(&mut self, motion: Motion, cx: &mut Context<Self>) -> bool {
        let command = match motion {
            Motion::Scroll(delta) => Command::ScrollFractional(delta),
            Motion::Selection(point) => Command::UpdateSelectionAt(point),
        };
        self.send_input(command, false, cx)
    }

    pub(super) fn flush_motion(&mut self, cx: &mut Context<Self>) -> bool {
        self.frame_motion
            .pending
            .take()
            .is_none_or(|motion| self.deliver_motion(motion, cx))
    }

    fn stage_motion(&mut self, motion: Motion, cx: &mut Context<Self>) {
        if let Some(prior) = self.frame_motion.push(motion) {
            self.deliver_motion(prior, cx);
        }
    }

    fn schedule_motion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.frame_motion.scheduled {
            self.frame_motion.scheduled = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.frame_motion.scheduled = false;
                this.flush_motion(cx);
            });
            cx.notify();
        }
    }

    pub(super) fn release_input(&mut self) {
        self.frame_motion.discard();
        self.link_press = None;
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
        if !self.preedit.is_empty() || self.paste_preview.is_some() {
            return;
        }
        let key = &event.keystroke;
        let mods = key.modifiers;
        if self.settings.shortcuts.action(key) == Some(rustterm::config::ShortcutAction::Search) {
            if !event.is_held {
                self.open_search(window, cx);
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
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
            self.open_keyboard_context_menu(window, cx);
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
            && !self.paste_preview.is_some()
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

    fn commit_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.accepts_text_input(window) && !text.is_empty() {
            if self.production_protected() && text.contains(['\r', '\n']) {
                self.paste_text(text.to_owned(), window, cx);
            } else {
                self.send(Command::Input(text.as_bytes().to_vec()), cx);
            }
        }
    }

    pub(super) fn begin_selection(
        &mut self,
        position: Point<Pixels>,
        kind: SelectionKind,
        cx: &mut Context<Self>,
    ) {
        self.begin_selection_with_intent(position, kind, SelectionIntent::New, cx);
    }

    pub(super) fn begin_selection_with_intent(
        &mut self,
        position: Point<Pixels>,
        kind: SelectionKind,
        intent: SelectionIntent,
        cx: &mut Context<Self>,
    ) {
        if self.chrome_blocked(cx) || self.show_final_alternate {
            return;
        }
        if self.mouse_position(position).is_none() {
            return;
        }
        let Some(point) = self.selection_position(position) else {
            return;
        };
        self.primary_copy_task.take();
        self.hovered_link = None;
        if intent != SelectionIntent::Preserve
            && !self.start_selection_at(point, kind, intent == SelectionIntent::Extend, cx)
        {
            self.selection_drag = None;
            self.selection_pointer = None;
            return;
        }
        self.selection_drag = Some(SelectionDrag::new(point, position, kind, intent));
        self.selection_pointer = Some(position);
    }

    fn start_selection_at(
        &mut self,
        position: SelectionPoint,
        kind: SelectionKind,
        extend: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.flush_motion(cx);
        self.send_input(
            Command::BeginSelectionAt {
                position,
                kind,
                extend,
            },
            false,
            cx,
        )
    }

    pub fn drag_selection(
        &mut self,
        position: Point<Pixels>,
        released: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_scrolling = self.selection_scroll_distance() != 0.;
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        if released {
            // Retry the final extent even if an earlier move hit a full queue.
            drag.last_update = None;
        }
        self.selection_pointer = Some(position);
        let scrolling = !released && self.selection_scroll_distance() != 0.;
        if !scrolling && let Some(drag) = &mut self.selection_drag {
            drag.scroll = DragScroll::default();
        }
        if scrolling != was_scrolling {
            self.poll_wake.notify_one();
        }
        if let Some(point) = self.selection_endpoint(position, cx) {
            self.stage_motion(Motion::Selection(point), cx);
            if !released {
                self.schedule_motion(window, cx);
            }
        }
        if released && let Some(drag) = self.selection_drag.take() {
            let delivered = self.flush_motion(cx);
            self.selection_pointer = None;
            match drag.finish() {
                SelectionFinish::Copy if delivered => {
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    self.copy_primary(cx);
                }
                SelectionFinish::Clear => self.send(Command::ClearSelection, cx),
                SelectionFinish::Copy | SelectionFinish::Keep => {}
            }
        }
    }

    /// Register during canvas paint. Element-level move handlers stop at the
    /// hitbox, but a selection belongs to its originating pane until release.
    pub(super) fn capture_selection_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let Some(view) = view.upgrade() else {
                return;
            };
            if view.read(cx).selection_drag.is_none() {
                return;
            }
            view.update(cx, |this, cx| {
                if event.pressed_button != Some(MouseButton::Left)
                    || !this.visible
                    || !window.is_window_active()
                    || this.chrome_blocked(cx)
                {
                    this.release_input();
                    cx.notify();
                    return;
                }
                if let Some(press) = &mut this.link_press {
                    press.update(event.position, event.modifiers);
                }
                this.drag_selection(event.position, false, window, cx);
                window.prevent_default();
                cx.stop_propagation();
            });
        });
        let view = cx.weak_entity();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                return;
            }
            let Some(view) = view.upgrade() else {
                return;
            };
            if view.read(cx).selection_drag.is_none() {
                return;
            }
            view.update(cx, |this, cx| {
                if !this.visible || !window.is_window_active() || this.chrome_blocked(cx) {
                    this.release_input();
                    cx.notify();
                    return;
                }
                this.finish_link(event.position, event.modifiers, cx);
                this.drag_selection(event.position, true, window, cx);
                window.prevent_default();
                cx.stop_propagation();
            });
        });
    }

    pub(super) fn selection_endpoint(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<SelectionPoint> {
        let point = self.selection_position(position)?;
        let snapshot = self.snapshot.as_ref()?;
        let drag = self.selection_drag.as_mut()?;
        if !drag.update(position, point, snapshot.version) {
            return None;
        }
        let begin = std::mem::take(&mut drag.deferred).then_some(drag.origin);
        if let Some(position) = begin
            && !self.start_selection_at(position, SelectionKind::Character, false, cx)
        {
            self.selection_drag = None;
            self.selection_pointer = None;
            return None;
        }
        Some(point)
    }

    fn selection_position(&self, position: Point<Pixels>) -> Option<SelectionPoint> {
        let bounds = self.bounds?;
        selection_hit(
            self.snapshot.as_ref()?,
            f32::from(position.x - bounds.left() - px(PADDING) - self.gutter_width()),
            f32::from(position.y - bounds.top() - px(PADDING)),
            f32::from(self.cell_width),
            f32::from(self.line_height),
        )
    }

    pub(super) fn selection_contains(&self, position: Point<Pixels>) -> bool {
        self.mouse_position(position)
            .and_then(|(column, row)| {
                self.snapshot
                    .as_ref()?
                    .lines
                    .get(usize::from(row))?
                    .cells
                    .get(usize::from(column))
            })
            .is_some_and(|cell| cell.selected)
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
        let (column, row) = (
            (x / width) as u16,
            (y / height + snapshot.scroll_fraction) as u16,
        );
        (column < snapshot.columns
            && usize::from(row) < snapshot.lines.len()
            && y < height * f32::from(snapshot.rows))
        .then_some((column, row))
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Even sub-cell deltas belong to this canvas; a parent must not consume
        // the same gesture while it is being accumulated here.
        cx.stop_propagation();
        if event.modifiers.control && !event.modifiers.alt && !event.modifiers.platform {
            self.flush_motion(cx);
            let size = self.wheel.zoom(event.delta, self.font_size);
            if size != self.font_size {
                self.font_size = size;
                cx.notify();
            }
            // Zoom belongs to the pane, including when a remote application
            // has enabled mouse reporting or alternate-screen scrolling.
            return;
        }
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
        if target == WheelTarget::History
            && let ScrollDelta::Pixels(delta) = event.delta
        {
            self.wheel = WheelState::default();
            let lines = f64::from(f32::from(delta.y) / f32::from(self.line_height).max(1.));
            if lines.is_finite() && lines != 0. {
                self.stage_motion(Motion::Scroll(lines), cx);
                self.schedule_motion(window, cx);
            }
            return;
        }
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
    fn frame_motion_respects_scroll_boundaries_and_copies_the_release_endpoint() {
        let mut term = rustterm::terminal::Terminal::new(16, 2, 8);
        term.feed(b"one\r\ntwo\r\nthree\r\nabcdef");
        let mut motion = FrameMotion::default();
        let mut emitted = Vec::new();
        for delta in [-1., -2., 0.25, 0.5] {
            emitted.extend(motion.push(Motion::Scroll(delta)));
        }
        emitted.extend(motion.pending.take());
        assert_eq!(emitted, [Motion::Scroll(-3.), Motion::Scroll(0.75)]);
        for m in emitted {
            if let Motion::Scroll(delta) = m {
                term.scroll_fractional(delta);
            }
        }
        assert_eq!(term.snapshot().scroll_fraction, 0.25);
        term.scroll_fractional(-1.);
        term.begin_selection_at(
            SelectionPoint::new(0, 1, SelectionSide::Left),
            SelectionKind::Character,
            false,
        );
        for col in [1, 4, 2] {
            assert!(
                motion
                    .push(Motion::Selection(SelectionPoint::new(
                        col,
                        1,
                        SelectionSide::Right
                    )))
                    .is_none()
            );
        }
        let Some(Motion::Selection(released)) = motion.pending.take() else {
            panic!("release lost")
        };
        term.update_selection_at(released);
        assert_eq!(term.selected_text().as_deref(), Some("abc"));
    }

    #[test]
    fn focus_click_preserves_a_range_while_double_click_shift_and_drag_have_their_normal_meaning() {
        let plain = Modifiers::default();
        assert_eq!(
            selection_intent(false, true, 1, plain),
            SelectionIntent::Preserve
        );
        assert_eq!(
            selection_intent(false, true, 2, plain),
            SelectionIntent::New
        );
        assert_eq!(selection_intent(true, true, 1, plain), SelectionIntent::New);
        assert_eq!(
            selection_intent(false, false, 1, plain),
            SelectionIntent::New
        );
        let shift = Modifiers {
            shift: true,
            ..plain
        };
        assert_eq!(
            selection_intent(false, true, 1, shift),
            SelectionIntent::Extend
        );
        assert_eq!(
            selection_intent(true, false, 1, shift),
            SelectionIntent::Extend
        );
        let origin = gpui::point(px(10.), px(10.));
        let cell = SelectionPoint::new(1, 0, SelectionSide::Left);
        let mut drag = SelectionDrag::new(
            cell,
            origin,
            SelectionKind::Character,
            SelectionIntent::Preserve,
        );
        assert!(!drag.update(origin, cell, 1));
        assert!(!drag.update(origin + gpui::point(px(1.), px(0.)), cell, 2));
        assert_eq!(drag.finish(), SelectionFinish::Keep);
        assert!(drag.update(origin + gpui::point(px(0.), px(10.)), cell, 2));
        // Starting a drag is decided before scrolling moves the viewport.
        assert!(std::mem::take(&mut drag.deferred));
        assert_eq!(drag.finish(), SelectionFinish::Copy);
        let click =
            SelectionDrag::new(cell, origin, SelectionKind::Character, SelectionIntent::New);
        assert_eq!(click.finish(), SelectionFinish::Clear);
        let shift_click = SelectionDrag::new(
            cell,
            origin,
            SelectionKind::Character,
            SelectionIntent::Extend,
        );
        assert_eq!(shift_click.finish(), SelectionFinish::Copy);
    }

    #[test]
    fn pointer_hit_testing_uses_glyph_midpoints_including_wide_characters_and_wrap_padding() {
        use SelectionSide::{Left, Right};
        use rustterm::terminal::Terminal;
        let mut terminal = Terminal::new(8, 4, 30);
        terminal.feed("a中😀e\u{301}".as_bytes());
        let snapshot = terminal.snapshot();
        for (x, column, side) in [
            (4., 0, Left),
            (5., 0, Right),
            (19., 1, Left),
            (20., 2, Right),
            (39., 3, Left),
            (40., 4, Right),
        ] {
            assert_eq!(
                selection_hit(&snapshot, x, 1., 10., 20.),
                Some(SelectionPoint::new(column, 0, side))
            );
        }
        terminal.begin_selection_at(
            selection_hit(&snapshot, 20., 1., 10., 20.).unwrap(),
            SelectionKind::Character,
            false,
        );
        terminal.update_selection_at(selection_hit(&snapshot, 56., 1., 10., 20.).unwrap());
        assert_eq!(terminal.selected_text().as_deref(), Some("😀e\u{301}"));
        terminal.feed(b"\x1bc1234567");
        terminal.feed("中".as_bytes());
        assert_eq!(
            selection_hit(&terminal.snapshot(), 79., 1., 10., 20.),
            Some(SelectionPoint::new(6, 0, Right))
        );
    }

    #[test]
    fn fractional_scroll_hit_testing_tracks_the_partially_visible_rows() {
        let mut terminal = rustterm::terminal::Terminal::new(16, 3, 20);
        terminal.feed(b"zero\r\none\r\ntwo\r\nthree\r\nfour");
        terminal.scroll_fractional(0.5);
        let snapshot = terminal.snapshot();
        for (y, row) in [(0., 0), (9., 0), (10., 1), (30., 2), (59., 3)] {
            assert_eq!(selection_hit(&snapshot, 0., y, 8., 20.).unwrap().row, row);
        }
        let start = selection_hit(&snapshot, 0., 59., 8., 20.).unwrap();
        let end = selection_hit(&snapshot, 32., 59., 8., 20.).unwrap();
        terminal.begin_selection_at(start, SelectionKind::Character, false);
        terminal.update_selection_at(end);
        assert_eq!(terminal.selected_text().as_deref(), Some("four"));
    }

    #[test]
    fn drag_scroll_is_reachable_inside_the_window_and_does_not_start_on_click_or_horizontal_selection()
     {
        use gpui::{point, size};
        let bounds = Bounds::new(point(px(50.), px(100.)), size(px(500.), px(400.)));
        let origin = point(px(200.), px(300.));
        let drag = SelectionDrag::new(
            SelectionPoint::new(10, 10, SelectionSide::Left),
            origin,
            SelectionKind::Character,
            SelectionIntent::New,
        );
        let distance = |y| drag.scroll_distance(point(origin.x, px(y)), bounds, px(20.));
        assert_eq!(distance(300.), 0.);
        let near = distance(139.);
        let edge = distance(100.);
        let outside = distance(40.);
        assert!(
            near > 0.,
            "edge scrolling must start before the pointer leaves a maximized window"
        );
        assert!(edge > near && outside > edge);
        assert_eq!(distance(461.), -near);
        assert_eq!(distance(500.), -edge);
        assert_eq!(distance(560.), -outside);
        for kind in [
            SelectionKind::Character,
            SelectionKind::Word,
            SelectionKind::Line,
        ] {
            let origin = point(px(200.), px(490.));
            let drag = SelectionDrag::new(
                SelectionPoint::new(10, 19, SelectionSide::Left),
                origin,
                kind,
                SelectionIntent::New,
            );
            assert_eq!(drag.scroll_distance(origin, bounds, px(20.)), 0.);
            assert_eq!(
                drag.scroll_distance(point(px(400.), origin.y), bounds, px(20.)),
                0.
            );
            assert_eq!(
                drag.scroll_distance(point(origin.x, px(489.)), bounds, px(20.)),
                0.
            );
            assert_eq!(
                drag.scroll_distance(point(origin.x, px(480.)), bounds, px(20.)),
                0.,
                "moving inward must not scroll outward"
            );
            assert!(drag.scroll_distance(point(origin.x, px(495.)), bounds, px(20.)) < 0.);
        }
    }

    #[test]
    fn drag_autoscroll_tracks_distance_and_elapsed_time_without_catching_up_after_a_pause() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let travel = |distance, interval: u64| {
            let mut scroll = DragScroll::default();
            assert_eq!(scroll.step(distance, start), 0);
            (1..=960 / interval)
                .map(|tick| scroll.step(distance, start + Duration::from_millis(tick * interval)))
                .sum::<i32>()
        };
        let near = travel(0.1, 16);
        let far = travel(6., 16);
        assert!((3..=5).contains(&near), "near edge: {near}");
        assert!(far > near * 5 && far <= 120);
        assert!((far - travel(6., 8)).abs() <= 1);
        assert_eq!(travel(-6., 16), -far);

        let mut scroll = DragScroll::default();
        assert_eq!(scroll.step(100., start), 0);
        assert!((1..=12).contains(&scroll.step(100., start + Duration::from_secs(30))));
        assert_eq!(scroll.step(-100., start + Duration::from_secs(31)), 0);
        assert_eq!(scroll.step(0., start + Duration::from_secs(32)), 0);
        assert_eq!(scroll.step(100., start + Duration::from_secs(33)), 0);
    }

    #[test]
    fn enclosed_selection_keeps_the_entire_alt_gesture_local() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        for mouse_reporting in [false, true] {
            for archived in [false, true] {
                for shift in [false, true] {
                    let modifiers = Modifiers { shift, ..alt };
                    for (clicks, kind) in [
                        (1, SelectionKind::Character),
                        (2, SelectionKind::Enclosed),
                        (3, SelectionKind::Line),
                    ] {
                        assert_eq!(
                            local_selection_kind(clicks, modifiers, mouse_reporting, archived),
                            Some(kind),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn selection_gestures_preserve_words_lines_and_application_mouse_input() {
        for (clicks, kind) in [
            (1, SelectionKind::Character),
            (2, SelectionKind::Word),
            (3, SelectionKind::Line),
        ] {
            let plain = Modifiers::default();
            assert_eq!(
                local_selection_kind(clicks, plain, false, false),
                Some(kind)
            );
            assert_eq!(local_selection_kind(clicks, plain, true, true), Some(kind));
            assert_eq!(local_selection_kind(clicks, plain, true, false), None);
            assert_eq!(
                local_selection_kind(
                    clicks,
                    Modifiers {
                        shift: true,
                        ..plain
                    },
                    true,
                    false
                ),
                Some(kind),
            );
        }
        for modifiers in [
            Modifiers {
                alt: true,
                control: true,
                ..Modifiers::default()
            },
            Modifiers {
                alt: true,
                platform: true,
                ..Modifiers::default()
            },
        ] {
            assert_eq!(local_selection_kind(2, modifiers, true, false), None);
            assert_eq!(
                local_selection_kind(2, modifiers, false, false),
                Some(SelectionKind::Word)
            );
        }
    }

    #[test]
    fn a_stationary_press_stays_empty_when_output_scrolls_under_the_pointer() {
        let mut terminal = rustterm::terminal::Terminal::new(20, 2, 16);
        terminal.feed(b"alpha\r\nlive");
        terminal.begin_selection(1, 0, SelectionKind::Character);
        let position = gpui::point(px(10.), px(10.));
        let mut drag = SelectionDrag::new(
            SelectionPoint::new(1, 0, SelectionSide::Left),
            position,
            SelectionKind::Character,
            SelectionIntent::New,
        );
        terminal.feed(b"\r\nnext");
        if drag.update(
            position,
            SelectionPoint::new(1, 0, SelectionSide::Left),
            terminal.snapshot().version,
        ) {
            terminal.update_selection(1, 0);
        }
        assert!(!terminal.snapshot().has_selection);
        assert_eq!(terminal.selected_text(), None);
        assert!(!drag.publish);
    }

    #[test]
    fn selection_drag_coalesces_cell_motion_without_losing_scroll_or_output_updates() {
        let origin = gpui::point(px(20.), px(30.));
        let moved = gpui::point(px(40.), px(30.));
        let mut drag = SelectionDrag::new(
            SelectionPoint::new(2, 3, SelectionSide::Left),
            origin,
            SelectionKind::Character,
            SelectionIntent::New,
        );
        assert!(!drag.update(origin, SelectionPoint::new(2, 3, SelectionSide::Left), 1));
        assert!(!drag.update(origin, SelectionPoint::new(2, 3, SelectionSide::Left), 2));
        assert!(!drag.publish);
        assert!(drag.update(moved, SelectionPoint::new(4, 3, SelectionSide::Left), 2));
        assert!(drag.publish);
        assert!(!drag.update(moved, SelectionPoint::new(4, 3, SelectionSide::Left), 2));
        assert!(drag.update(moved, SelectionPoint::new(4, 3, SelectionSide::Left), 3));
        drag.scrolled();
        assert!(drag.update(moved, SelectionPoint::new(4, 3, SelectionSide::Left), 3));
        assert!(!drag.update(moved, SelectionPoint::new(4, 3, SelectionSide::Left), 3));
    }

    #[test]
    fn selection_drag_ignores_click_jitter_without_delaying_word_or_enclosed_selection() {
        let origin = gpui::point(px(29.5), px(39.5));
        let mut drag = SelectionDrag::new(
            SelectionPoint::new(2, 3, SelectionSide::Left),
            origin,
            SelectionKind::Character,
            SelectionIntent::New,
        );
        let jitter = gpui::point(px(30.5), px(40.5));
        assert!(!drag.update(jitter, SelectionPoint::new(3, 4, SelectionSide::Left), 1));
        assert!(!drag.publish);
        assert!(drag.update(
            gpui::point(px(33.), px(39.5)),
            SelectionPoint::new(3, 3, SelectionSide::Left),
            1
        ));
        assert!(drag.publish);
        // Once dragging, returning to the starting cell is an intentional selection.
        assert!(drag.update(origin, SelectionPoint::new(2, 3, SelectionSide::Left), 1));
        for kind in [
            SelectionKind::Word,
            SelectionKind::Enclosed,
            SelectionKind::Line,
        ] {
            let mut drag = SelectionDrag::new(
                SelectionPoint::new(2, 3, SelectionSide::Left),
                origin,
                kind,
                SelectionIntent::New,
            );
            assert!(drag.update(origin, SelectionPoint::new(2, 3, SelectionSide::Left), 1));
            assert!(drag.publish);
        }
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
    fn wheel_zoom_handles_mouse_touchpad_and_font_limits() {
        let mut wheel = WheelState::default();
        let lines = |y| ScrollDelta::Lines(Point { x: 0., y });
        assert_eq!(wheel.zoom(lines(3.), 13.), 14.);
        assert_eq!(wheel.zoom(lines(-3.), 13.), 12.);
        assert_eq!(wheel.zoom(lines(3.), 40.), 40.);
        assert_eq!(wheel.zoom(lines(-3.), 8.), 8.);
        assert_eq!(
            wheel.zoom(ScrollDelta::Lines(Point { x: 3., y: 0. }), 13.),
            13.
        );
        let pixels = ScrollDelta::Pixels(Point {
            x: px(0.),
            y: px(10.),
        });
        for _ in 0..3 {
            assert_eq!(wheel.zoom(pixels, 13.), 13.);
        }
        assert_eq!(wheel.zoom(pixels, 13.), 14.);
    }

    #[test]
    fn wheel_zoom_does_not_share_partial_gestures_with_terminal_scrolling() {
        let cell = Point {
            x: px(8.),
            y: px(16.),
        };
        let delta = ScrollDelta::Lines(Point { x: 0., y: 0.75 });
        for target in [
            WheelTarget::History,
            WheelTarget::Mouse,
            WheelTarget::Alternate,
        ] {
            let mut wheel = WheelState::default();
            assert_eq!(wheel.steps(delta, cell, target).y, 0);
            assert_eq!(wheel.zoom(delta, 13.), 13.);
            assert_eq!(wheel.steps(delta, cell, target).y, 0);
            assert_eq!(wheel.steps(delta, cell, target).y, 1);
        }
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
