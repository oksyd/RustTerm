use super::{InputEvent, TextInput};
use crate::app::ui;
use gpui::{AnyElement, Context, IntoElement, MouseButton, TextRun, div, prelude::*, px, rgb};
use rustterm::{
    config::SqlEngine,
    database::sql::{KEYWORDS, Kind, Token, lex},
};
use std::{
    collections::VecDeque,
    ops::Range,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Snapshot {
    pub text: String,
    pub anchor: usize,
    pub cursor: usize,
}

#[derive(Default)]
pub(super) struct History {
    undo: VecDeque<Snapshot>,
    redo: Vec<Snapshot>,
    typing: Option<(usize, Instant)>,
}

impl History {
    pub fn record(&mut self, before: Snapshot, after_cursor: usize, typing: bool) {
        let now = Instant::now();
        let grouped = typing
            && self.typing.is_some_and(|(cursor, time)| {
                cursor == before.cursor
                    && before.anchor == before.cursor
                    && now.duration_since(time) < Duration::from_millis(750)
            });
        self.redo.clear();
        if !grouped {
            self.undo.push_back(before);
            while self.undo.len() > 100
                || self
                    .undo
                    .iter()
                    .map(|snapshot| snapshot.text.len())
                    .sum::<usize>()
                    > 1024 * 1024
            {
                self.undo.pop_front();
            }
        }
        self.typing = typing.then_some((after_cursor, now));
    }

    pub fn boundary(&mut self) {
        self.typing = None;
    }

    pub fn restore(&mut self, current: Snapshot, redo: bool) -> Option<Snapshot> {
        self.boundary();
        if redo {
            let next = self.redo.pop()?;
            self.undo.push_back(current);
            Some(next)
        } else {
            let previous = self.undo.pop_back()?;
            self.redo.push(current);
            Some(previous)
        }
    }
}

pub(super) struct Completion {
    pub range: Range<usize>,
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

pub(super) struct Editor {
    pub engine: SqlEngine,
    pub tokens: Vec<Token>,
    pub history: History,
    pub completion: Option<Completion>,
    identifiers: Vec<String>,
}

impl Editor {
    pub fn new(engine: SqlEngine, text: &str) -> Self {
        Self {
            engine,
            tokens: lex(text, engine),
            history: History::default(),
            completion: None,
            identifiers: Vec::new(),
        }
    }

    pub fn refresh(&mut self, text: &str) {
        self.tokens = lex(text, self.engine);
        self.completion = None;
    }

    pub fn set_identifiers(&mut self, mut names: Vec<String>) {
        names.sort_unstable();
        names.dedup();
        self.identifiers = names;
        self.completion = None;
    }

    pub fn complete(&mut self, text: &str, cursor: usize) {
        let token = self
            .tokens
            .iter()
            .find(|token| token.range.start < cursor && cursor <= token.range.end);
        if token.is_some_and(|token| matches!(token.kind, Kind::String | Kind::Comment)) {
            self.completion = None;
            return;
        }
        let range = token
            .filter(|token| matches!(token.kind, Kind::Keyword | Kind::Identifier))
            .map_or(cursor..cursor, |token| token.range.clone());
        let prefix = text[range.start..cursor]
            .trim_start_matches(['"', '`', '['])
            .to_lowercase();
        let qualified = text[..range.start].ends_with('.');
        let mut items = Vec::new();
        for name in &self.identifiers {
            let name_tokens = lex(name, self.engine);
            let part = name_tokens
                .iter()
                .rev()
                .find(|token| matches!(token.kind, Kind::Identifier | Kind::Keyword))
                .map_or(name.as_str(), |token| &name[token.range.clone()]);
            let candidate = part.trim_matches(['"', '`', '[', ']']).to_lowercase();
            if candidate.starts_with(&prefix)
                || (!qualified && name.to_lowercase().starts_with(&prefix))
            {
                items.push((
                    name.clone(),
                    if qualified {
                        part.to_owned()
                    } else {
                        name.clone()
                    },
                ));
            }
        }
        if !qualified {
            items.extend(
                KEYWORDS
                    .iter()
                    .filter(|keyword| keyword.to_lowercase().starts_with(&prefix))
                    .map(|keyword| ((*keyword).to_owned(), (*keyword).to_owned())),
            );
        }
        items.truncate(200);
        self.completion = (!items.is_empty()).then_some(Completion {
            range,
            items,
            selected: 0,
        });
    }
}

impl TextInput {
    pub(super) fn sql_key_down(
        &mut self,
        key: &str,
        command: bool,
        shift: bool,
        alt: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.sql.is_none() || alt {
            return false;
        }
        if command && key == "space" {
            if !self.read_only {
                self.sql
                    .as_mut()
                    .unwrap()
                    .complete(&self.content, self.cursor);
                cx.notify();
            }
            return true;
        }
        if command && matches!(key, "z" | "y") {
            if !self.read_only {
                let current = Snapshot {
                    text: self.content.to_string(),
                    anchor: self.anchor,
                    cursor: self.cursor,
                };
                if let Some(snapshot) = self
                    .sql
                    .as_mut()
                    .unwrap()
                    .history
                    .restore(current, key == "y" || shift)
                {
                    self.content = zeroize::Zeroizing::new(snapshot.text);
                    self.anchor = snapshot.anchor;
                    self.cursor = snapshot.cursor;
                    self.reveal_cursor = true;
                    self.validation_error = None;
                    self.sql.as_mut().unwrap().refresh(&self.content);
                    cx.emit(InputEvent::Changed);
                    cx.notify();
                }
            }
            return true;
        }
        if command || shift {
            return false;
        }
        let Some(completion) = self
            .sql
            .as_mut()
            .and_then(|editor| editor.completion.as_mut())
        else {
            return false;
        };
        match key {
            "up" => completion.selected = completion.selected.saturating_sub(1),
            "down" => {
                completion.selected = (completion.selected + 1).min(completion.items.len() - 1)
            }
            "enter" | "tab" => {
                let index = completion.selected;
                self.accept_sql_completion(index, cx);
            }
            "escape" => self.sql.as_mut().unwrap().completion = None,
            _ => return false,
        }
        cx.notify();
        true
    }

    fn accept_sql_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.read_only || self.is_composing() {
            return;
        }
        let Some(completion) = self
            .sql
            .as_mut()
            .and_then(|editor| editor.completion.take())
        else {
            return;
        };
        let Some((_, insertion)) = completion.items.get(index) else {
            return;
        };
        if self.replace(completion.range, insertion, cx) {
            cx.emit(InputEvent::Changed);
            cx.notify();
        }
    }

    pub(super) fn sql_runs(&self, range: Range<usize>, base: &TextRun) -> Vec<TextRun> {
        let Some(editor) = &self.sql else {
            return vec![base.clone()];
        };
        if range.is_empty() {
            return vec![base.clone()];
        }
        editor
            .tokens
            .iter()
            .filter_map(|token| {
                let start = token.range.start.max(range.start);
                let end = token.range.end.min(range.end);
                (start < end).then(|| {
                    let mut run = base.clone();
                    run.len = end - start;
                    run.color = rgb(match token.kind {
                        Kind::Keyword => ui::ACCENT,
                        Kind::String => ui::SUCCESS,
                        Kind::Number => ui::BRAND,
                        Kind::Comment => ui::FAINT,
                        Kind::Operator => ui::MUTED,
                        _ => ui::TEXT,
                    })
                    .into();
                    run
                })
            })
            .collect()
    }

    pub(super) fn render_sql_completions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let completion = self.sql.as_ref()?.completion.as_ref()?;
        let bounds = self.bounds?;
        let line = self
            .layout
            .iter()
            .find(|line| line.range.start <= self.cursor && self.cursor <= line.range.end)?;
        let height = (completion.items.len().min(6) * 24 + 8) as f32;
        let first = completion.selected.saturating_sub(5);
        let left = (f32::from(self.line_x(line, self.cursor) - self.scroll_x) + 9.)
            .clamp(0., (f32::from(bounds.size.width) - 260.).max(0.));
        let top = (line.row as f32 * 20. - f32::from(self.scroll_y) + 25.)
            .clamp(5., (175. - height).max(5.));
        Some(
            div()
                .id("sql-completion-menu")
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(260.))
                .max_w_full()
                .py(px(4.))
                .bg(rgb(ui::RAISED))
                .border_1()
                .border_color(rgb(ui::BORDER))
                .rounded(px(4.))
                .shadow_md()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .children(completion.items.iter().enumerate().skip(first).take(6).map(
                    |(index, (label, _))| {
                        div()
                            .id(("sql-completion", index))
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .text_size(px(12.))
                            .text_color(rgb(ui::TEXT))
                            .cursor_pointer()
                            .when(index == completion.selected, |row| {
                                row.bg(rgb(ui::SELECTED))
                            })
                            .hover(|style| style.bg(rgb(ui::HOVER)))
                            .child(div().min_w_0().truncate().child(label.clone()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.accept_sql_completion(index, cx)
                            }))
                    },
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_coalesces_typing_and_invalidates_redo_after_edit() {
        let snapshot = |text: &str| Snapshot {
            text: text.into(),
            anchor: text.len(),
            cursor: text.len(),
        };
        let mut history = History::default();
        history.record(snapshot(""), 1, true);
        history.record(snapshot("s"), 2, true);
        assert_eq!(history.restore(snapshot("se"), false).unwrap().text, "");
        assert_eq!(history.restore(snapshot(""), true).unwrap().text, "se");
        history.record(snapshot("se"), 3, false);
        assert!(history.restore(snapshot("sea"), true).is_none());
    }

    #[test]
    fn completion_avoids_literals_and_supports_qualified_names() {
        let mut editor = Editor::new(SqlEngine::Postgres, "SELECT u.na");
        editor.set_identifiers(vec!["\"name\"".into(), "public.users".into()]);
        editor.complete("SELECT u.na", 11);
        let completion = editor.completion.as_ref().unwrap();
        assert_eq!(completion.range, 9..11);
        assert_eq!(completion.items[0].1, "\"name\"");
        editor.refresh("SELECT 'na'");
        editor.complete("SELECT 'na'", 10);
        assert!(editor.completion.is_none());
    }

    #[test]
    fn qualified_completion_preserves_dots_inside_quoted_identifiers() {
        let mut editor = Editor::new(SqlEngine::Postgres, "SELECT t.od");
        editor.set_identifiers(vec!["\"odd.name\"".into()]);
        editor.complete("SELECT t.od", 11);
        assert_eq!(
            editor.completion.as_ref().unwrap().items[0].1,
            "\"odd.name\""
        );
    }
}
