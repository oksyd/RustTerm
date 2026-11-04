//! Display-only semantic spans. Cache keys include exact text, so overwritten rows
//! cannot inherit decoration. The grid's explicit ANSI foreground always wins.
mod rules;

use super::{Snapshot, Terminal};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::Flags,
};
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};

const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_CACHE_BYTES: usize = 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 256;
#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    range: Range<usize>,
    color: u32,
}
#[derive(Default)]
pub(super) struct HighlightCache {
    entries: HashMap<String, Arc<[Span]>>,
    order: VecDeque<String>,
    bytes: usize,
}
impl HighlightCache {
    fn spans(&mut self, text: &str, theme: &super::Theme) -> Arc<[Span]> {
        if text.len() > MAX_LINE_BYTES {
            return Arc::default();
        }
        if let Some(spans) = self.entries.get(text) {
            return spans.clone();
        }
        let mut spans = rules::spans(text, theme);
        color_brackets(text, &mut spans, theme);
        let spans: Arc<[Span]> = spans.into();
        let charge = text.len() * 2 + spans.len() * std::mem::size_of::<Span>() + 128;
        while self.bytes + charge > MAX_CACHE_BYTES || self.entries.len() >= MAX_CACHE_ENTRIES {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&key) {
                self.bytes = self
                    .bytes
                    .saturating_sub(key.len() * 2 + old.len() * std::mem::size_of::<Span>() + 128);
            }
        }
        self.bytes += charge;
        self.order.push_back(text.into());
        self.entries.insert(text.into(), spans.clone());
        spans
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

fn color_brackets(text: &str, spans: &mut Vec<Span>, theme: &super::Theme) {
    let colors = [
        theme.warn,
        theme.address,
        theme.info,
        theme.url,
        theme.debug,
        theme.error,
    ];
    let mut stack = Vec::new();
    let mut brackets = Vec::new();
    let mut span_index = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        while spans
            .get(span_index)
            .is_some_and(|span| span.range.end <= index)
        {
            span_index += 1;
        }
        if spans
            .get(span_index)
            .is_some_and(|span| span.range.contains(&index))
        {
            escaped = false;
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        let apostrophe = ch == '\''
            && text[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            && text[index + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        if let Some(delimiter) = quote {
            if ch == delimiter && !apostrophe {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' && !apostrophe {
            quote = Some(ch);
            continue;
        }
        match ch {
            '(' | '[' | '{' => stack.push((index, ch, colors[stack.len() % colors.len()])),
            ')' | ']' | '}' => {
                if let Some((start, opening, color)) = stack.pop()
                    && matches!((opening, ch), ('(', ')') | ('[', ']') | ('{', '}'))
                {
                    brackets.push(Span {
                        range: start..start + 1,
                        color,
                    });
                    brackets.push(Span {
                        range: index..index + 1,
                        color,
                    });
                } else {
                    stack.clear();
                }
            }
            _ => {}
        }
    }
    spans.extend(brackets);
    spans.sort_unstable_by_key(|span| span.range.start);
}

impl Terminal {
    pub fn set_highlight(&mut self, enabled: bool) {
        self.highlight_enabled = enabled;
        self.version = self.version.wrapping_add(1);
    }
    pub(super) fn decorate_snapshot(&self, snapshot: &mut Snapshot) {
        if !self.highlight_enabled {
            return;
        }
        let grid = self.term.grid();
        let view_top = -(grid.display_offset() as i32);
        let view_end = view_top + self.size.rows as i32;
        let mut current = view_top;
        let mut cache = self
            .highlight_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while current < view_end {
            let mut first = current;
            let mut estimated = 0;
            while first > grid.topmost_line().0
                && grid[Point::new(Line(first - 1), Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                first -= 1;
                estimated += self.size.columns as usize;
                if estimated > MAX_LINE_BYTES {
                    break;
                }
            }
            let mut text = String::new();
            let mut map = Vec::new();
            let mut line = first;
            let mut overlong = estimated > MAX_LINE_BYTES;
            loop {
                for column in 0..self.size.columns as usize {
                    let cell = &grid[Point::new(Line(line), Column(column))];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let start = text.len();
                    if cell.flags.contains(Flags::HIDDEN) {
                        text.push(' ');
                    } else {
                        text.push(cell.c);
                        for &c in cell.zerowidth().unwrap_or_default() {
                            text.push(c);
                        }
                    }
                    if line >= view_top && line < view_end {
                        map.push((line, column, start..text.len()));
                    }
                    if text.len() > MAX_LINE_BYTES {
                        overlong = true;
                        break;
                    }
                }
                let wrapped = grid[Point::new(Line(line), Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE);
                line += 1;
                if overlong || !wrapped || line > grid.bottommost_line().0 {
                    break;
                }
            }
            if !overlong {
                let spans = cache.spans(&text, &self.theme);
                let mut span_index = 0;
                for (line, column, range) in map {
                    while spans
                        .get(span_index)
                        .is_some_and(|span| span.range.end <= range.start)
                    {
                        span_index += 1;
                    }
                    let cell = &mut snapshot.lines[(line - view_top) as usize].cells[column];
                    if cell.default_foreground
                        && !cell.hidden
                        && let Some(span) = spans.get(span_index)
                        && span.range.start < range.end
                    {
                        cell.foreground = if grid[Point::new(Line(line), Column(column))]
                            .flags
                            .contains(Flags::DIM)
                        {
                            super::dim(span.color)
                        } else {
                            span.color
                        };
                    }
                }
            }
            current = line.max(current + 1);
        }
    }
}

#[cfg(test)]
mod tests;
