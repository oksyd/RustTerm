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
    underline: bool,
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
    let colors = theme.bracket_colors();
    let pairs = super::delimiters::pairs(text, spans.iter().map(|span| span.range.clone()));
    for pair in pairs.into_iter().filter(|pair| !pair.quoted) {
        let color = colors[pair.depth % colors.len()];
        for offset in [pair.open, pair.close] {
            spans.push(Span {
                range: offset..offset + 1,
                color,
                underline: false,
            });
        }
    }
    spans.sort_unstable_by_key(|span| span.range.start);
}

impl Terminal {
    pub fn set_highlight(&mut self, enabled: bool) {
        self.highlight_enabled = enabled;
        self.version = self.version.wrapping_add(1);
    }
    pub(super) fn decorate_snapshot(&self, snapshot: &mut Snapshot, reused: &[bool]) {
        if !self.highlight_enabled {
            return;
        }
        let grid = self.term.grid();
        let mut visible = 0;
        let mut cache = self
            .highlight_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while visible < snapshot.lines.len() {
            if reused[visible] {
                visible += 1;
                continue;
            }
            let current = snapshot.lines[visible].source_line.0;
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
                    if let Ok(row) = snapshot
                        .lines
                        .binary_search_by_key(&Line(line), |row| row.source_line)
                    {
                        map.push((row, line, column, start..text.len()));
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
                for (row, line, column, range) in map {
                    if reused[row] {
                        continue;
                    }
                    while spans
                        .get(span_index)
                        .is_some_and(|span| span.range.end <= range.start)
                    {
                        span_index += 1;
                    }
                    let cell = &mut Arc::make_mut(&mut snapshot.lines[row].cells)[column];
                    if !cell.hidden
                        && let Some(span) = spans.get(span_index)
                        && span.range.start < range.end
                    {
                        let flags = grid[Point::new(Line(line), Column(column))].flags;
                        if cell.default_foreground {
                            cell.foreground = if flags.contains(Flags::DIM) {
                                super::dim(span.color)
                            } else {
                                span.color
                            };
                        }
                        if span.underline
                            && !snapshot.alternate_screen
                            && !snapshot.mouse_reporting
                            && !flags.contains(Flags::INVERSE)
                        {
                            cell.underline = true;
                        }
                    }
                }
            }
            visible += 1;
            while visible < snapshot.lines.len() && snapshot.lines[visible].source_line.0 < line {
                visible += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
