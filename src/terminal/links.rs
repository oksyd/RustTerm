use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLink {
    Url(String),
    RemotePath(String),
}

impl Snapshot {
    pub fn link_at(&self, column: usize, row: usize) -> Option<TerminalLink> {
        if self.alternate_screen || self.mouse_reporting {
            return None;
        }
        let clicked = self.lines.get(row)?.cells.get(column)?;
        if clicked.hidden {
            return None;
        }
        let mut first = row;
        while first > 0
            && self.lines[first - 1].wrapped
            && self.lines[first - 1].source_line.0 + 1 == self.lines[first].source_line.0
        {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < self.lines.len()
            && self.lines[last].wrapped
            && self.lines[last].source_line.0 + 1 == self.lines[last + 1].source_line.0
        {
            last += 1;
        }
        let mut text = String::new();
        let mut hit = None;
        let mut previous = 0;
        for (index, line) in self.lines.iter().enumerate().take(last + 1).skip(first) {
            for (col, cell) in line.cells.iter().enumerate() {
                if cell.width != 0 {
                    previous = text.len();
                }
                if index == row && col == column {
                    hit = Some(previous);
                }
                if cell.width != 0 {
                    text.push_str(if cell.hidden { " " } else { &cell.text });
                }
                if text.len() > 16 * 1024 {
                    return None;
                }
            }
        }
        let hit = hit?;
        let token = token_at(&text, hit)?.trim_start_matches(['(', '[', '{', '<']);
        if token.split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        }) {
            let mut token = token.trim_end_matches([',', ';', '.', '>']);
            while let Some(last) = token.chars().last() {
                let open = match last {
                    ')' => '(',
                    ']' => '[',
                    '}' => '{',
                    _ => break,
                };
                if token.matches(last).count() <= token.matches(open).count() {
                    break;
                }
                token = &token[..token.len() - last.len_utf8()];
            }
            return is_web_url(token).then(|| TerminalLink::Url(token.to_owned()));
        }
        let mut path = token;
        for _ in 0..2 {
            if let Some((prefix, suffix)) = path.rsplit_once(':')
                && !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_digit())
            {
                path = prefix;
            }
        }
        if !path.contains('/')
            || path.contains("://")
            || path.len() > 4096
            || path.chars().any(char::is_control)
        {
            return None;
        }
        if path.starts_with('/') {
            return Some(TerminalLink::RemotePath(path.to_owned()));
        }
        let cwd = self
            .command_blocks
            .iter()
            .find(|b| b.rows.contains(&row))?
            .details
            .cwd
            .as_deref()?;
        if !cwd.starts_with('/') || path.starts_with('~') {
            return None;
        }
        Some(TerminalLink::RemotePath(format!(
            "{}/{path}",
            cwd.trim_end_matches('/')
        )))
    }
}

pub(super) fn is_web_url(value: &str) -> bool {
    url::Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn token_at(text: &str, hit: usize) -> Option<&str> {
    let mut start = None;
    let mut quote = None;
    for (index, ch) in text.char_indices() {
        if start.is_none() {
            if ch.is_whitespace() {
                continue;
            }
            if matches!(ch, '\'' | '"') {
                quote = Some(ch);
                start = Some(index + 1);
                continue;
            }
            start = Some(index);
        }
        if quote == Some(ch) || (quote.is_none() && ch.is_whitespace()) {
            let begin = start.take()?;
            if begin <= hit && hit < index {
                return Some(&text[begin..index]);
            }
            quote = None;
        }
    }
    let start = start?;
    (quote.is_none() && start <= hit && hit < text.len()).then_some(&text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_preserve_balanced_url_punctuation_and_literal_path_names() {
        let mut terminal = Terminal::new(100, 8, 100);
        terminal.feed(b"(https://example.com/f(x)).\r\nhttp://[::1]\r\n'/tmp/trailing.'\r\n");
        let snapshot = terminal.snapshot();
        assert_eq!(
            snapshot.link_at(3, 0),
            Some(TerminalLink::Url("https://example.com/f(x)".into()))
        );
        assert_eq!(
            snapshot.link_at(3, 1),
            Some(TerminalLink::Url("http://[::1]".into()))
        );
        assert_eq!(
            snapshot.link_at(3, 2),
            Some(TerminalLink::RemotePath("/tmp/trailing.".into()))
        );
    }

    #[test]
    fn links_resolve_wrapped_unicode_and_command_directory_without_guessing() {
        let mut terminal = Terminal::new(30, 12, 100);
        terminal.feed("\x1b]7;file://host/project\x07\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07中文 https://example.com/abcdefghijk\r\n./src/main.rs:12:4\r\n\x1b]7;file://host/elsewhere\x07\x1b]133;D;0\x07".as_bytes());
        let snapshot = terminal.snapshot();
        assert_eq!(
            snapshot.link_at(8, 1),
            Some(TerminalLink::Url("https://example.com/abcdefghijk".into()))
        );
        assert_eq!(snapshot.link_at(2, 2), snapshot.link_at(8, 1));
        assert_eq!(
            snapshot.link_at(5, 3),
            Some(TerminalLink::RemotePath("/project/./src/main.rs".into()))
        );
        let mut plain = Terminal::new(80, 8, 100);
        plain.feed(b"src/main.rs\r\n/tmp/log.txt\r\nhttps://example.com/x).\r\n'./a b.txt'\r\n");
        let plain = plain.snapshot();
        assert_eq!(plain.link_at(2, 0), None);
        assert_eq!(
            plain.link_at(2, 1),
            Some(TerminalLink::RemotePath("/tmp/log.txt".into()))
        );
        assert_eq!(
            plain.link_at(2, 2),
            Some(TerminalLink::Url("https://example.com/x".into()))
        );
        assert_eq!(plain.link_at(6, 3), None);
    }
}
