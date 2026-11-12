//! IME-aware form input with an optional multiline mode for text values.
//! Password text is only rendered when explicitly revealed, and is never exposed
//! to the clipboard, IME surrounding text or emitted events.

use rustterm::i18n::messages as i18n_key;
mod sql;

use crate::app::ui;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, ShapedLine, SharedString,
    Subscription, TextRun, UTF16Selection, Window, canvas, div, fill, font, point, prelude::*, px,
    rgb, size,
};
use rustterm::i18n::tr;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroizing;

const MAX_INPUT_BYTES: usize = 65_536;

pub(super) fn can_edit_literal(value: &str) -> bool {
    value.len() <= MAX_INPUT_BYTES && supports_literal_text(value, true)
}

pub(super) fn can_edit_inline_literal(value: &str) -> bool {
    value.len() <= MAX_INPUT_BYTES && supports_literal_text(value, false)
}

pub(super) fn alt_gr_text(key: &gpui::Keystroke) -> Option<&str> {
    if key.modifiers.control && key.modifiers.alt && !key.modifiers.platform {
        key.key_char.as_deref()
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Changed,
    Submit,
    Rejected(&'static str),
}

struct InputLine {
    range: Range<usize>,
    row: usize,
    shaped: ShapedLine,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputSize {
    Inline,
    Compact,
    Normal,
    Large,
}

pub struct TextInput {
    focus: FocusHandle,
    content: Zeroizing<String>,
    placeholder: SharedString,
    password: bool,
    password_visibility_toggle: bool,
    password_revealed: bool,
    read_only: bool,
    borderless: bool,
    size: InputSize,
    multiline: bool,
    multiline_height: f32,
    literal: bool,
    reveal_cursor: bool,
    anchor: usize,
    cursor: usize,
    marked: Option<Range<usize>>,
    original: Option<(Zeroizing<String>, usize, usize)>,
    bounds: Option<Bounds<Pixels>>,
    layout: Vec<InputLine>,
    scroll_x: Pixels,
    scroll_y: Pixels,
    selecting: bool,
    blur_subscription: Option<Subscription>,
    focus_subscription: Option<Subscription>,
    activation_subscription: Option<Subscription>,
    scroll_container: Option<gpui::ScrollHandle>,
    scroll_on_focus: bool,
    validation_error: Option<rustterm::i18n::Message>,
    invalid: bool,
    sql: Option<sql::Editor>,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(
        placeholder: impl Into<SharedString>,
        password: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            // GPUI uses the tracked handle's tab-stop flag, not the div's.
            focus: cx.focus_handle().tab_stop(true),
            content: Zeroizing::new(String::new()),
            placeholder: placeholder.into(),
            password,
            password_visibility_toggle: false,
            password_revealed: false,
            read_only: false,
            borderless: false,
            size: InputSize::Normal,
            multiline: false,
            multiline_height: 180.,
            literal: false,
            reveal_cursor: true,
            anchor: 0,
            cursor: 0,
            marked: None,
            original: None,
            bounds: None,
            layout: Vec::new(),
            scroll_x: px(0.),
            scroll_y: px(0.),
            selecting: false,
            blur_subscription: None,
            focus_subscription: None,
            activation_subscription: None,
            scroll_container: None,
            scroll_on_focus: false,
            validation_error: None,
            invalid: false,
            sql: None,
        }
    }

    /// Callers must not log this value, especially for credential inputs.
    pub fn value(&self) -> &str {
        self.original
            .as_ref()
            .map(|original| original.0.as_str())
            .unwrap_or(&self.content)
    }

    pub fn selected_text(&self) -> Option<&str> {
        let selected = self.selection();
        (!self.password && !selected.is_empty()).then(|| &self.content[selected])
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.is_composing() {
            return;
        }
        self.anchor = 0;
        self.move_cursor(self.content.len(), true, cx);
    }

    pub fn set_multiline_height(&mut self, height: f32, cx: &mut Context<Self>) {
        let height = height.clamp(80., 2000.);
        if (self.multiline_height - height).abs() > 0.5 {
            self.multiline_height = height;
            self.reveal_cursor = true;
            cx.notify();
        }
    }

    pub fn set_sql_dialect(&mut self, engine: rustterm::config::SqlEngine, cx: &mut Context<Self>) {
        if self.password {
            return;
        }
        self.literal = true;
        self.multiline = true;
        if self.size == InputSize::Inline {
            self.size = InputSize::Normal;
        }
        self.sql = Some(sql::Editor::new(engine, &self.content));
        self.reveal_cursor = true;
        cx.notify();
    }

    pub fn set_sql_completions(&mut self, identifiers: Vec<String>, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.sql {
            editor.set_identifiers(identifiers);
            cx.notify();
        }
    }

    pub fn execution_sql(&self) -> Result<&str, &'static str> {
        if self.is_composing() {
            return Err(i18n_key::DATABASE_FINISH_COMPOSING_TEXT_BEFORE_RUNNING_SQL);
        }
        if let Some(selected) = self.selected_text() {
            return Ok(selected);
        }
        if let Some(editor) = &self.sql {
            let range = rustterm::database::sql::statement_range(
                &self.content,
                self.cursor,
                editor.engine,
            )?;
            Ok(&self.content[range])
        } else {
            Ok(self.value())
        }
    }

    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.is_composing() {
            return;
        }
        if self.replace(self.selection(), text, cx) {
            cx.emit(InputEvent::Changed);
            cx.notify();
        }
    }

    pub fn set_value(&mut self, value: impl AsRef<str>, cx: &mut Context<Self>) {
        let value = value.as_ref();
        if value.len() > MAX_INPUT_BYTES {
            return;
        }
        let Some(text) = self.prepare(value) else {
            return;
        };
        self.content = text;
        self.hide_password();
        self.validation_error = None;
        self.invalid = false;
        self.reveal_cursor = true;
        self.cursor = self.content.len();
        self.anchor = self.cursor;
        self.marked = None;
        self.original = None;
        if let Some(editor) = &mut self.sql {
            editor.refresh(&self.content);
            editor.history = sql::History::default();
        }
        cx.notify();
    }

    pub fn set_validation_error(
        &mut self,
        message: rustterm::i18n::Message,
        cx: &mut Context<Self>,
    ) {
        self.validation_error = Some(message);
        self.scroll_on_focus = true;
        cx.notify();
    }

    pub fn clear_validation_error(&mut self, cx: &mut Context<Self>) {
        if self.validation_error.take().is_some() {
            cx.notify();
        }
    }

    /// Mark a field whose error message is rendered by its containing form.
    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid != invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_value("", cx);
    }

    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only != read_only {
            if read_only {
                self.cancel_composition();
                self.hide_password();
            }
            self.read_only = read_only;
            cx.notify();
        }
    }

    /// Embed the input in a composite control that draws its own focus border.
    pub fn set_borderless(&mut self, borderless: bool, cx: &mut Context<Self>) {
        self.borderless = borderless;
        cx.notify();
    }

    /// Use compact geometry without changing selection, focus or composition.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        self.set_size(
            if compact {
                InputSize::Compact
            } else {
                InputSize::Normal
            },
            cx,
        );
    }

    pub fn set_large(&mut self, large: bool, cx: &mut Context<Self>) {
        self.set_size(
            if large {
                InputSize::Large
            } else {
                InputSize::Normal
            },
            cx,
        );
    }

    fn set_size(&mut self, size: InputSize, cx: &mut Context<Self>) {
        if self.size != size {
            self.size = size;
            cx.notify();
        }
    }

    pub fn set_password_visibility_toggle(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.password_visibility_toggle = enabled && self.password;
        if !self.password_visibility_toggle {
            self.hide_password();
        }
        cx.notify();
    }

    fn hide_password(&mut self) {
        if self.password_revealed {
            self.password_revealed = false;
            self.layout.clear();
        }
    }

    fn password_masked(&self) -> bool {
        self.password && !self.password_revealed
    }
    /// Literal data rejects unsupported control characters instead of normalizing them.
    /// Set this before assigning a value; ordinary form fields keep their existing policy.
    pub fn set_literal(&mut self, multiline: bool, cx: &mut Context<Self>) {
        self.literal = true;
        self.multiline = multiline && !self.password;
        if self.size == InputSize::Inline {
            self.size = InputSize::Normal;
        }
        self.reveal_cursor = true;
        self.layout.clear();
        self.scroll_y = px(0.);
        cx.notify();
    }

    /// Configure a single-line cell editor before assigning its value.
    /// Newlines and control characters are rejected without changing the original text.
    pub fn set_inline_literal(&mut self, cx: &mut Context<Self>) {
        self.set_literal(false, cx);
        self.sql = None;
        self.size = InputSize::Inline;
    }

    fn prepare(&self, text: &str) -> Option<Zeroizing<String>> {
        if self.literal {
            literal_text(text, self.multiline)
        } else {
            Some(prepare_text(text, self.password))
        }
    }

    pub fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }
    pub fn set_scroll_container(&mut self, scroll: gpui::ScrollHandle) {
        self.scroll_container = Some(scroll);
    }
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub fn is_composing(&self) -> bool {
        self.marked.is_some()
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn cancel_composition(&mut self) {
        if let Some((content, anchor, cursor)) = self.original.take() {
            self.content = content;
            self.anchor = anchor;
            self.cursor = cursor;
        }
        self.marked = None;
        self.reveal_cursor = true;
        if let Some(editor) = &mut self.sql {
            editor.refresh(&self.content);
        }
    }

    fn cancel_interaction(&mut self, cx: &mut Context<Self>) {
        self.cancel_composition();
        self.selecting = false;
        if let Some(editor) = &mut self.sql {
            editor.history.boundary();
        }
        cx.notify();
    }

    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) -> bool {
        if self.read_only {
            return false;
        }
        if text.len() > MAX_INPUT_BYTES {
            cx.emit(InputEvent::Rejected(
                i18n_key::REDIS_TEXT_IS_LIMITED_TO_64_KIB,
            ));
            return false;
        }
        let Some(text) = self.prepare(text) else {
            cx.emit(InputEvent::Rejected(
                if self.size == InputSize::Inline && text.contains(['\n', '\r', '\t']) {
                    i18n_key::DATABASE_USE_THE_EXPANDED_EDITOR_FOR_MULTILINE_TEXT
                } else {
                    i18n_key::REDIS_THIS_TEXT_CONTAINS_CONTROL_CHARACTERS_USE_ESCAPED_TEXT_TO_PRESERVE
                },
            ));
            return false;
        };
        if self.content.len() - range.len() + text.len() > MAX_INPUT_BYTES {
            cx.emit(InputEvent::Rejected(
                i18n_key::REDIS_TEXT_IS_LIMITED_TO_64_KIB,
            ));
            return false;
        }
        let mut next = String::with_capacity(self.content.len() - range.len() + text.len());
        next.push_str(&self.content[..range.start]);
        next.push_str(&text);
        next.push_str(&self.content[range.end..]);
        if let Some(editor) = &mut self.sql
            && self.original.is_none()
            && next != self.content.as_str()
        {
            editor.history.record(
                sql::Snapshot {
                    text: self.content.to_string(),
                    anchor: self.anchor,
                    cursor: self.cursor,
                },
                range.start + text.len(),
                range.is_empty() && text.chars().count() == 1 && !text.contains('\n'),
            );
        }
        self.content = Zeroizing::new(next); // The replaced allocation is cleared before release.
        self.validation_error = None;
        self.cursor = range.start + text.len();
        self.anchor = self.cursor;
        self.reveal_cursor = true;
        if let Some(editor) = &mut self.sql {
            let completing = editor.completion.is_some() && self.original.is_none();
            editor.refresh(&self.content);
            if completing {
                editor.complete(&self.content, self.cursor);
            }
        }
        true
    }

    fn move_cursor(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.sql {
            editor.completion = None;
            editor.history.boundary();
        }
        self.cursor = offset;
        self.reveal_cursor = true;
        if !extend {
            self.anchor = offset;
        }
        cx.notify();
    }

    fn previous(&self) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < self.cursor).then_some(i))
            .unwrap_or(0)
    }
    fn next(&self) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > self.cursor).then_some(i))
            .unwrap_or(self.content.len())
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !window.is_window_active() || !self.focus.is_focused(window) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if self.marked.is_some() {
            if key == "escape" {
                self.cancel_composition();
                cx.stop_propagation();
                window.prevent_default();
                cx.notify();
            }
            return;
        }
        let selected = self.selection();
        // Linux does not commit Ctrl+Alt text automatically. Handle the actual
        // character before its normalized key can trigger a clipboard shortcut.
        if let Some(text) = alt_gr_text(&event.keystroke) {
            if self.replace(selected, text, cx) {
                cx.emit(InputEvent::Changed);
                cx.notify();
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        let command = modifiers.control || modifiers.platform;
        if self.sql_key_down(key, command, modifiers.shift, modifiers.alt, cx) {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if command {
            match key {
                "enter" if self.literal && !modifiers.shift && !modifiers.alt => {
                    if !self.read_only {
                        cx.emit(InputEvent::Submit);
                    }
                }
                "home" if self.multiline => self.move_cursor(0, modifiers.shift, cx),
                "end" if self.multiline => {
                    self.move_cursor(self.content.len(), modifiers.shift, cx)
                }
                "a" => self.select_all(cx),
                "c" | "x" => {
                    if !self.password && !selected.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.content[selected.clone()].to_owned(),
                        ));
                        if key == "x" && self.replace(selected, "", cx) {
                            cx.emit(InputEvent::Changed);
                            cx.notify();
                        }
                    }
                }
                "v" => {
                    if !self.read_only
                        && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        && self.replace(selected, &text, cx)
                    {
                        cx.emit(InputEvent::Changed);
                        cx.notify();
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        match key {
            "enter" => {
                if self.multiline {
                    if self.replace(selected, "\n", cx) {
                        cx.emit(InputEvent::Changed);
                        cx.notify();
                    }
                } else if !self.read_only {
                    cx.emit(InputEvent::Submit);
                }
            }
            "up" | "down" if self.multiline => {
                let target = vertical_offset(&self.content, self.cursor, key == "down");
                self.move_cursor(target, modifiers.shift, cx);
            }
            "left" => {
                let target = if !modifiers.shift && !selected.is_empty() {
                    selected.start
                } else {
                    self.previous()
                };
                self.move_cursor(target, modifiers.shift, cx);
            }
            "right" => {
                let target = if !modifiers.shift && !selected.is_empty() {
                    selected.end
                } else {
                    self.next()
                };
                self.move_cursor(target, modifiers.shift, cx);
            }
            "home" => {
                let start = if self.multiline {
                    line_range(&self.content, self.cursor).start
                } else {
                    0
                };
                self.move_cursor(start, modifiers.shift, cx);
            }
            "end" => {
                let end = if self.multiline {
                    line_range(&self.content, self.cursor).end
                } else {
                    self.content.len()
                };
                self.move_cursor(end, modifiers.shift, cx);
            }
            "backspace" | "delete" => {
                let range = if !selected.is_empty() {
                    selected
                } else if key == "backspace" {
                    self.previous()..self.cursor
                } else {
                    self.cursor..self.next()
                };
                if self.replace(range, "", cx) {
                    cx.emit(InputEvent::Changed);
                    cx.notify();
                }
            }
            _ => return, // Printable text arrives once through EntityInputHandler.
        }
        cx.stop_propagation();
        window.prevent_default();
    }

    fn display_index(&self, byte: usize) -> usize {
        if self.password_masked() {
            self.content[..byte].chars().count() * '•'.len_utf8()
        } else {
            byte
        }
    }
    fn content_index(&self, display_byte: usize) -> usize {
        if self.password_masked() {
            self.content
                .char_indices()
                .nth(display_byte / '•'.len_utf8())
                .map(|(byte, _)| byte)
                .unwrap_or(self.content.len())
        } else {
            let mut byte = display_byte.min(self.content.len());
            while !self.content.is_char_boundary(byte) {
                byte -= 1;
            }
            byte
        }
    }
    fn line_height(&self, bounds: Bounds<Pixels>) -> Pixels {
        if self.multiline {
            px(20.)
        } else {
            bounds.size.height.max(px(1.))
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.bounds else {
            return self.cursor;
        };
        if self.content.is_empty() {
            return 0;
        }
        let row = if self.multiline {
            (f32::from(position.y - bounds.top() + self.scroll_y) / 20.).max(0.) as usize
        } else {
            0
        };
        let Some(line) = self.layout.iter().min_by_key(|line| line.row.abs_diff(row)) else {
            return self.cursor;
        };
        let index = line
            .shaped
            .closest_index_for_x(position.x - bounds.left() + self.scroll_x);
        let byte = if self.multiline {
            line.range.start + index.min(line.range.len())
        } else {
            self.content_index(index)
        };
        grapheme_boundary(&self.content, byte)
    }

    fn line_x(&self, line: &InputLine, index: usize) -> Pixels {
        let byte = if self.multiline {
            index.clamp(line.range.start, line.range.end) - line.range.start
        } else {
            self.display_index(index)
        };
        line.shaped.x_for_index(byte)
    }

    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        self.bounds = Some(bounds);
        let empty = self.content.is_empty();
        let height = self.line_height(bounds);
        let cursor_row = if self.multiline {
            self.content[..self.cursor]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
        } else {
            0
        };
        let line_count = if self.multiline {
            self.content.bytes().filter(|byte| *byte == b'\n').count() + 1
        } else {
            1
        };
        let reveal = std::mem::take(&mut self.reveal_cursor);
        if reveal {
            let y = height * cursor_row as f32;
            self.scroll_y = self
                .scroll_y
                .min(y)
                .max(y + height - bounds.size.height)
                .max(px(0.));
        }
        self.scroll_y = self.scroll_y.clamp(
            px(0.),
            (height * line_count as f32 - bounds.size.height).max(px(0.)),
        );
        let first = (f32::from(self.scroll_y) / f32::from(height)).max(0.) as usize;
        let visible = (f32::from(bounds.size.height) / f32::from(height)).ceil() as usize + 1;
        self.layout.clear();
        let mut offset = 0;
        for (row, text) in self.content.split('\n').enumerate() {
            let range = offset..offset + text.len();
            offset = range.end + 1;
            if self.multiline && row < first {
                continue;
            }
            if self.multiline && row >= first + visible {
                break;
            }
            let text: SharedString = if empty {
                tr(self.placeholder.as_ref()).to_owned().into()
            } else if self.password_masked() {
                "•".repeat(self.content.chars().count()).into()
            } else if self.multiline {
                text.to_owned().into()
            } else {
                self.content.to_string().into()
            };
            let run = TextRun {
                len: text.len(),
                font: font(if self.multiline || self.size == InputSize::Inline {
                    crate::app::ui::MONOSPACE_FONT
                } else {
                    ".SystemUIFont"
                }),
                color: rgb(if empty {
                    ui::colors().faint
                } else {
                    ui::colors().text
                })
                .into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = if empty {
                vec![run]
            } else {
                self.sql_runs(range.clone(), &run)
            };
            let shaped = window.text_system().shape_line(
                text,
                px(if self.multiline {
                    13.
                } else {
                    match self.size {
                        InputSize::Inline | InputSize::Normal => 12.,
                        InputSize::Compact => 11.,
                        InputSize::Large => 14.,
                    }
                }),
                &runs,
                None,
            );
            self.layout.push(InputLine {
                range: if self.multiline {
                    range
                } else {
                    0..self.content.len()
                },
                row,
                shaped,
            });
            if !self.multiline {
                break;
            }
        }
        if reveal && let Some(line) = self.layout.iter().find(|line| line.row == cursor_row) {
            let cursor_x = self.line_x(line, self.cursor);
            self.scroll_x = self
                .scroll_x
                .min(cursor_x)
                .max(cursor_x - bounds.size.width + px(3.))
                .max(px(0.));
        }
        if empty {
            self.scroll_x = px(0.);
        } else if !self.multiline {
            self.scroll_x = self.scroll_x.min(self.horizontal_scroll_limit(bounds));
        }
    }

    fn horizontal_scroll_limit(&self, bounds: Bounds<Pixels>) -> Pixels {
        if self.content.is_empty() {
            return px(0.);
        }
        let width = self
            .layout
            .iter()
            .map(|line| line.shaped.width)
            .max()
            .unwrap_or(px(0.));
        (width + px(3.) - bounds.size.width).max(px(0.))
    }

    fn scroll(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds else { return };
        let delta = event.delta.pixel_delta(px(20.));
        let x = scrolled_offset(self.scroll_x, delta.x, self.horizontal_scroll_limit(bounds));
        let y = if self.multiline {
            let lines = self.content.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let limit = (px(20.) * lines as f32 - bounds.size.height).max(px(0.));
            scrolled_offset(self.scroll_y, delta.y, limit)
        } else {
            self.scroll_y
        };
        if x != self.scroll_x || y != self.scroll_y {
            self.scroll_x = x;
            self.scroll_y = y;
            self.reveal_cursor = false;
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn scroll_metrics(&self, axis: ui::ScrollAxis) -> ui::ScrollMetrics {
        let Some(bounds) = self.bounds.filter(|_| self.multiline) else {
            return ui::ScrollMetrics::default();
        };
        match axis {
            ui::ScrollAxis::Horizontal => ui::ScrollMetrics {
                viewport: f32::from(bounds.size.width),
                content: f32::from(bounds.size.width + self.horizontal_scroll_limit(bounds)),
                offset: f32::from(self.scroll_x),
            },
            ui::ScrollAxis::Vertical => {
                let lines = self.content.bytes().filter(|byte| *byte == b'\n').count() + 1;
                ui::ScrollMetrics {
                    viewport: f32::from(bounds.size.height),
                    content: lines as f32 * f32::from(self.line_height(bounds)),
                    offset: f32::from(self.scroll_y),
                }
            }
        }
    }

    fn render_scrollbar(&self, axis: ui::ScrollAxis, cx: &Context<Self>) -> gpui::AnyElement {
        let input = cx.entity().downgrade();
        let metrics_input = input.clone();
        ui::scrollbar(
            match axis {
                ui::ScrollAxis::Horizontal => "input-horizontal-scrollbar",
                ui::ScrollAxis::Vertical => "input-vertical-scrollbar",
            },
            axis,
            move |cx| {
                metrics_input
                    .upgrade()
                    .map(|input| input.read(cx).scroll_metrics(axis))
                    .unwrap_or_default()
            },
            move |offset, _, cx| {
                let _ = input.update(cx, |input, cx| {
                    let metrics = input.scroll_metrics(axis);
                    let offset = px(offset.clamp(0., (metrics.content - metrics.viewport).max(0.)));
                    match axis {
                        ui::ScrollAxis::Horizontal => input.scroll_x = offset,
                        ui::ScrollAxis::Vertical => input.scroll_y = offset,
                    }
                    input.reveal_cursor = false;
                    cx.notify();
                });
            },
        )
        .into_any_element()
    }

    fn scroll_into_view(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        if !std::mem::take(&mut self.scroll_on_focus) {
            return;
        }
        let Some(scroll) = &self.scroll_container else {
            return;
        };
        let viewport = scroll.bounds();
        let top = viewport.top() + px(6.);
        let bottom = viewport.bottom() - px(6.);
        let delta = if bounds.top() < top
            || (self.validation_error.is_some() && bounds.size.height > bottom - top)
        {
            top - bounds.top()
        } else if bounds.bottom() > bottom {
            bottom - bounds.bottom()
        } else {
            px(0.)
        };
        let mut offset = scroll.offset();
        offset.y = (offset.y + delta).clamp(-scroll.max_offset().height, px(0.));
        if offset != scroll.offset() {
            let scroll = scroll.clone();
            window.on_next_frame(move |window, _| {
                scroll.set_offset(offset);
                window.refresh();
            });
        }
    }

    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let selected = self.selection();
        let height = self.line_height(bounds);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for line in &self.layout {
                let origin = point(
                    bounds.left() - self.scroll_x,
                    bounds.top() + height * line.row as f32 - self.scroll_y,
                );
                if !selected.is_empty()
                    && selected.start <= line.range.end
                    && selected.end > line.range.start
                {
                    let left = origin.x + self.line_x(line, selected.start.max(line.range.start));
                    let mut right = origin.x + self.line_x(line, selected.end.min(line.range.end));
                    if selected.end > line.range.end {
                        right += px(8.);
                    }
                    window.paint_quad(fill(
                        Bounds::new(point(left, origin.y), size(right - left, height)),
                        rgb(ui::colors().selected),
                    ));
                }
                let _ = line.shaped.paint(origin, height, window, cx);
                if let Some(marked) = &self.marked
                    && marked.start <= line.range.end
                    && marked.end >= line.range.start
                {
                    let left = origin.x + self.line_x(line, marked.start.max(line.range.start));
                    let right = origin.x + self.line_x(line, marked.end.min(line.range.end));
                    window.paint_quad(fill(
                        Bounds::new(
                            point(left, origin.y + height - px(2.)),
                            size(right - left, px(1.)),
                        ),
                        rgb(ui::colors().accent),
                    ));
                }
                if self.focus.is_focused(window)
                    && line.range.start <= self.cursor
                    && self.cursor <= line.range.end
                {
                    let x = origin.x + self.line_x(line, self.cursor);
                    window.paint_quad(fill(
                        Bounds::new(point(x, origin.y), size(px(1.), height)),
                        rgb(ui::colors().accent),
                    ));
                }
            }
        });
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.blur_subscription.is_none() {
            self.blur_subscription = Some(cx.on_blur(&self.focus, window, |this, _, cx| {
                this.cancel_interaction(cx);
            }));
            self.scroll_on_focus = self.focus.is_focused(window);
            self.focus_subscription = Some(cx.on_focus(&self.focus, window, |this, _, cx| {
                this.scroll_on_focus = true;
                this.reveal_cursor = true;
                cx.notify();
            }));
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |this, window, cx| {
                    if !window.is_window_active() {
                        // Roll back locally before rejecting late XIM commits.
                        // Window deactivation does not clear the logical focus.
                        this.hide_password();
                        this.cancel_interaction(cx);
                    }
                }));
        }
        let input = cx.entity();
        let paint_input = input.clone();
        let height = if self.multiline {
            self.multiline_height
        } else {
            match self.size {
                InputSize::Inline => ui::metrics::ROW,
                InputSize::Compact => ui::metrics::CONTROL_COMPACT,
                InputSize::Normal => ui::metrics::INPUT,
                InputSize::Large => 44.,
            }
        };
        let horizontal_padding = match self.size {
            InputSize::Inline | InputSize::Compact => 7.,
            InputSize::Normal => 9.,
            InputSize::Large => 12.,
        };
        let vertical_padding = match self.size {
            InputSize::Inline | InputSize::Compact => 3.,
            InputSize::Normal => 5.,
            InputSize::Large => 10.,
        };
        let completions = self.render_sql_completions(cx);
        let field = div()
            .relative()
            .w_full()
            .h(px(height))
            .when(self.multiline, |d| d.flex_shrink_0())
            .min_w_0()
            .px(px(horizontal_padding))
            .when(self.password_visibility_toggle, |d| d.pr(px(42.)))
            .py(px(vertical_padding))
            .when(self.multiline, |field| {
                field
                    .pr(px(horizontal_padding + ui::SCROLLBAR_WIDTH))
                    .pb(px(5. + ui::SCROLLBAR_WIDTH))
            })
            .rounded(px(match self.size {
                InputSize::Inline => 0.,
                InputSize::Large => 6.,
                _ => ui::metrics::RADIUS,
            }))
            .border_1()
            .border_color(rgb(if self.invalid || self.validation_error.is_some() {
                crate::app::ui::colors().danger
            } else if self.focus.is_focused(window) {
                ui::colors().accent
            } else {
                ui::colors().border
            }))
            .when(self.borderless, |d| d.border_0().rounded_none())
            .bg(rgb(crate::app::ui::colors().input))
            .when(self.read_only, |d| d.opacity(0.65))
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus)
            .tab_index(0)
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.scroll(event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus.focus(window);
                    this.cancel_composition();
                    this.selecting = true;
                    let offset = this.index_at(event.position);
                    if event.click_count >= 2 {
                        this.anchor = 0;
                        this.cursor = this.content.len();
                        cx.notify();
                    } else {
                        this.move_cursor(offset, event.modifiers.shift, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.selecting && event.pressed_button == Some(MouseButton::Left) {
                    this.move_cursor(this.index_at(event.position), true, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        input.update(cx, |this, _| {
                            this.layout(bounds, window);
                            if this.validation_error.is_none() {
                                this.scroll_into_view(bounds, window);
                            }
                        });
                    },
                    move |bounds, _, window, cx| {
                        paint_input.update(cx, |this, cx| {
                            if !this.read_only {
                                window.handle_input(
                                    &this.focus,
                                    ElementInputHandler::new(bounds, paint_input.clone()),
                                    cx,
                                );
                            }
                            this.paint(bounds, window, cx);
                        });
                    },
                )
                .size_full()
                // A percentage height can collapse to zero in the SQL pane's
                // flex layout. The editor already has a fixed outer height.
                .when(self.multiline, |canvas| {
                    canvas.h(px(height
                        - vertical_padding
                        - 5.
                        - ui::SCROLLBAR_WIDTH
                        - 2.))
                }),
            )
            .when(self.multiline, |field| {
                field
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right(px(ui::SCROLLBAR_WIDTH))
                            .child(self.render_scrollbar(ui::ScrollAxis::Horizontal, cx)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom(px(ui::SCROLLBAR_WIDTH))
                            .left_0()
                            .right_0()
                            .child(self.render_scrollbar(ui::ScrollAxis::Vertical, cx)),
                    )
            })
            .when(self.password_visibility_toggle, |field| {
                let (icon, label) = if self.password_revealed {
                    (
                        crate::app::ui::Icon::EyeOff,
                        i18n_key::SETTINGS_HIDE_PASSWORD,
                    )
                } else {
                    (crate::app::ui::Icon::Eye, i18n_key::SETTINGS_SHOW_PASSWORD)
                };
                field.child(
                    crate::app::ui::icon_button_enabled(
                        "password-visibility",
                        icon,
                        label,
                        !self.read_only,
                    )
                    .absolute()
                    .right(px(6.))
                    .top(px((height - 28.) / 2. - 1.))
                    .rounded(px(4.))
                    .focus(|style| style.bg(rgb(crate::app::ui::colors().hover)))
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(|this, event, window, cx| {
                        if this.read_only || this.is_composing() {
                            return;
                        }
                        this.password_revealed = !this.password_revealed;
                        this.layout.clear();
                        this.reveal_cursor = true;
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            this.focus.focus(window);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    })),
                )
            })
            .children(completions);
        if let Some(message) = &self.validation_error {
            let error_input = cx.entity();
            div()
                .relative()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(field)
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::app::ui::colors().danger))
                        .child(message.render()),
                )
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            error_input.update(cx, |this, _| {
                                this.scroll_into_view(bounds, window);
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .into_any_element()
        } else {
            field.into_any_element()
        }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.content, range);
        let mapped = to_utf16(&self.content, range.clone());
        *actual = Some(mapped.clone());
        if self.password {
            Some("•".repeat(mapped.len()))
        } else {
            Some(self.content[range].to_owned())
        }
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16(&self.content, self.selection()),
            reversed: self.cursor < self.anchor,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .clone()
            .map(|range| to_utf16(&self.content, range))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_composition();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only || !window.is_window_active() || !self.focus.is_focused(window) {
            return;
        }
        let explicit_range = range.is_some();
        let range = range
            .map(|range| from_utf16(&self.content, range))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection());
        // GPUI Wayland sends an explicit empty replacement of the marked range when
        // composition is cancelled or the window loses focus. Restore the original
        // selection instead of committing a deletion of the text it had replaced.
        if explicit_range && text.is_empty() && self.marked.as_ref() == Some(&range) {
            self.cancel_composition();
            cx.notify();
            return;
        }
        let composition =
            self.sql
                .as_ref()
                .and(self.original.as_ref())
                .map(|(text, anchor, cursor)| sql::Snapshot {
                    text: text.to_string(),
                    anchor: *anchor,
                    cursor: *cursor,
                });
        if self.replace(range, text, cx) {
            if let Some(snapshot) = composition
                && let Some(editor) = &mut self.sql
            {
                editor.history.record(snapshot, self.cursor, false);
            }
            self.marked = None;
            self.original = None;
            cx.emit(InputEvent::Changed);
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
        if self.read_only || !window.is_window_active() || !self.focus.is_focused(window) {
            return;
        }
        if text.chars().any(char::is_control) {
            if self.literal {
                cx.emit(InputEvent::Rejected(
                    i18n_key::REDIS_THIS_TEXT_CONTAINS_CONTROL_CHARACTERS_USE_ESCAPED_TEXT_TO_PRESERVE,
                ));
            }
            return;
        }
        if text.is_empty() {
            self.cancel_composition();
            cx.notify();
            return;
        }
        let range = range
            .map(|range| from_utf16(&self.content, range))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection());
        // Reject an oversized preedit before taking a rollback snapshot. Otherwise value()
        // would keep returning that stale snapshot although no composition was installed.
        if text.len() > MAX_INPUT_BYTES
            || self.content.len() - range.len() + text.len() > MAX_INPUT_BYTES
        {
            cx.emit(InputEvent::Rejected(
                i18n_key::REDIS_TEXT_IS_LIMITED_TO_64_KIB,
            ));
            return;
        }
        if self.original.is_none() {
            self.original = Some((
                Zeroizing::new(self.content.to_string()),
                self.anchor,
                self.cursor,
            ));
        }
        let start = range.start;
        if self.replace(range, text, cx) {
            self.marked = Some(start..start + text.len());
            if let Some(selected) = selected {
                let selected = from_utf16(text, selected);
                self.anchor = start + selected.start;
                self.cursor = start + selected.end;
            }
            cx.notify(); // Composition does not emit Changed/Submit to the form.
        }
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds?;
        let range = from_utf16(&self.content, range);
        let line = self
            .layout
            .iter()
            .find(|line| line.range.start <= range.start && range.start <= line.range.end)?;
        let left = (bounds.left() + self.line_x(line, range.start) - self.scroll_x)
            .clamp(bounds.left(), bounds.right());
        let right = (bounds.left() + self.line_x(line, range.end.min(line.range.end))
            - self.scroll_x)
            .clamp(left, bounds.right());
        let height = self.line_height(bounds);
        Some(Bounds::new(
            point(
                left,
                bounds.top() + height * line.row as f32 - self.scroll_y,
            ),
            size((right - left).max(px(1.)), height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.content[..self.index_at(point)].encode_utf16().count())
    }
}

fn scrolled_offset(current: Pixels, delta: Pixels, limit: Pixels) -> Pixels {
    if delta == px(0.) {
        current
    } else {
        (current - delta).clamp(px(0.), limit)
    }
}

fn supports_literal_text(text: &str, multiline: bool) -> bool {
    !text
        .chars()
        .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
}

fn literal_text(text: &str, multiline: bool) -> Option<Zeroizing<String>> {
    supports_literal_text(text, multiline).then(|| Zeroizing::new(text.to_owned()))
}

fn line_range(text: &str, cursor: usize) -> Range<usize> {
    let start = text[..cursor].rfind('\n').map_or(0, |offset| offset + 1);
    let end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |offset| cursor + offset);
    start..end
}

fn vertical_offset(text: &str, cursor: usize, down: bool) -> usize {
    let current = line_range(text, cursor);
    let column = text[current.start..cursor].graphemes(true).count();
    let next = if down && current.end < text.len() {
        line_range(text, current.end + 1)
    } else if !down && current.start > 0 {
        line_range(text, current.start - 1)
    } else {
        return cursor;
    };
    text[next.clone()]
        .grapheme_indices(true)
        .nth(column)
        .map_or(next.end, |(offset, _)| next.start + offset)
}

fn prepare_text(text: &str, password: bool) -> Zeroizing<String> {
    // Passwords are opaque values: trimming or normalizing even a newline changes the
    // credential. They remain visually single-line because only bullets are shaped.
    Zeroizing::new(if password {
        text.to_owned()
    } else {
        single_line(text)
    })
}

fn grapheme_boundary(text: &str, byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index <= byte).then_some(index))
        .unwrap_or(0)
}

fn single_line(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}
fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    fn offset(text: &str, target: usize) -> usize {
        let mut count = 0;
        for (index, ch) in text.char_indices() {
            if count >= target {
                return index;
            }
            count += ch.len_utf16();
        }
        text.len()
    }
    let start = offset(text, range.start);
    start..offset(text, range.end).max(start)
}
fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wheel_axes_stay_independent_and_stop_at_content_edges() {
        assert_eq!(scrolled_offset(px(25.), px(0.), px(10.)), px(25.));
        assert_eq!(scrolled_offset(px(0.), px(30.), px(100.)), px(0.));
        assert_eq!(scrolled_offset(px(100.), px(-30.), px(100.)), px(100.));
        assert_eq!(scrolled_offset(px(25.), px(-4.), px(100.)), px(29.));
        assert_eq!(scrolled_offset(px(25.), px(4.), px(100.)), px(21.));
        assert_eq!(scrolled_offset(px(0.), px(-30.), px(0.)), px(0.));
    }

    #[test]
    fn utf16_ranges_keep_surrogates_and_cjk_valid() {
        let text = "a😀中e\u{301}";
        assert_eq!(&text[from_utf16(text, 1..3)], "😀");
        assert_eq!(&text[from_utf16(text, 3..4)], "中");
        assert!(text.is_char_boundary(from_utf16(text, 2..2).start));
        assert_eq!(to_utf16(text, 1..5), 1..3);
    }
    #[test]
    fn form_paste_cannot_insert_control_sequences_or_newlines() {
        assert_eq!(single_line("a\n中\t\x1b[31m\0"), "a 中 [31m");
    }
    #[test]
    fn passwords_are_not_silently_normalized() {
        let password = " leading\t中\r\n\0trailing ";
        assert_eq!(prepare_text(password, true).as_str(), password);
        assert_eq!(prepare_text("name\nvalue", false).as_str(), "name value");
    }
    #[test]
    fn literal_editors_preserve_text_and_reject_lossy_pastes() {
        let text = "  {\"s\":\"中\\path\"}\n\tsecond line  ";
        assert_eq!(literal_text(text, true).unwrap().as_str(), text);
        assert!(literal_text(text, false).is_none());
        for control in ["a\0b", "a\rb", "a\u{85}b"] {
            assert!(literal_text(control, true).is_none());
        }
        assert_eq!(
            literal_text("  unquoted\\value  ", false).unwrap().as_str(),
            "  unquoted\\value  "
        );
    }

    #[test]
    fn inline_literal_eligibility_preserves_whitespace_and_rejects_multiline() {
        for value in ["", "  +001.2300  ", "中😀\\path", "NULL"] {
            assert!(can_edit_inline_literal(value));
            assert_eq!(literal_text(value, false).unwrap().as_str(), value);
        }
        for value in ["a\nb", "a\rb", "a\tb", "a\0b", "a\x1bb"] {
            assert!(!can_edit_inline_literal(value));
        }
        assert!(can_edit_inline_literal(&"x".repeat(MAX_INPUT_BYTES)));
        assert!(!can_edit_inline_literal(&"x".repeat(MAX_INPUT_BYTES + 1)));
        assert!(can_edit_literal("a\nb"));
    }

    #[test]
    fn multiline_navigation_keeps_graphemes_and_line_edges() {
        let text = "a😀中\ne\u{301}z\n\nlast";
        let second = text.find('e').unwrap();
        assert_eq!(vertical_offset(text, 5, true), second + "e\u{301}z".len());
        assert_eq!(vertical_offset(text, second, false), 0);
        assert_eq!(line_range(text, second), second..second + "e\u{301}z".len());
        assert_eq!(line_range("first\n", 6), 6..6);
        assert_eq!(vertical_offset("first\n", 6, false), 0);
        assert_eq!(vertical_offset("first\n", 6, true), 6);
    }

    #[test]
    fn pointer_selection_cannot_split_a_grapheme() {
        let text = "e\u{301}😀x";
        assert_eq!(grapheme_boundary(text, 1), 0);
        assert_eq!(grapheme_boundary(text, 3), 3);
        assert_eq!(grapheme_boundary(text, 7), 7);
        assert_eq!(grapheme_boundary(text, usize::MAX), text.len());
    }
}
