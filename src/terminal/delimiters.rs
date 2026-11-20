//! Display-text delimiters shared by bracket coloring and local selection.
use std::ops::Range;

pub(super) struct Pair {
    pub open: usize,
    pub close: usize,
    pub depth: usize,
    pub quoted: bool,
}

/// Callers bound the input size. Protected tokens (URLs, paths, etc.) are
/// opaque to the highlighter; selection can scan the same text without them.
pub(super) fn pairs(text: &str, protected: impl Iterator<Item = Range<usize>>) -> Vec<Pair> {
    let mut protected = protected.peekable();
    let mut stack = Vec::new();
    let mut result = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        while protected.peek().is_some_and(|range| range.end <= index) {
            protected.next();
        }
        if protected.peek().is_some_and(|range| range.contains(&index)) {
            escaped = false;
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
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
        if let Some((open, delimiter)) = quote {
            if ch == delimiter && !apostrophe {
                result.push(Pair {
                    open,
                    close: index,
                    depth: stack.len(),
                    quoted: true,
                });
                quote = None;
            }
            continue;
        }
        if matches!(ch, '"' | '\x60') || ch == '\'' && !apostrophe {
            quote = Some((index, ch));
            continue;
        }
        match ch {
            '(' | '[' | '{' => stack.push((index, ch)),
            ')' | ']' | '}' => {
                if let Some((open, opening)) = stack.pop()
                    && matches!((opening, ch), ('(', ')') | ('[', ']') | ('{', '}'))
                {
                    result.push(Pair {
                        open,
                        close: index,
                        depth: stack.len(),
                        quoted: false,
                    });
                } else {
                    stack.clear();
                }
            }
            _ => {}
        }
    }
    result
}
