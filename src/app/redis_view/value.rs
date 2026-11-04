//! Lossless presentation for Redis bytes and explicit text/escaped editor modes.

use rustterm::i18n::messages as i18n_key;
const PREVIEW_BYTES: usize = 4096;
pub(super) const EDIT_BYTES: usize = 60 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EditEncoding {
    Text,
    Escaped,
}

impl EditEncoding {
    pub(super) fn encode(self, bytes: &[u8], multiline: bool) -> Result<String, &'static str> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR)?;
        let encoded = match self {
            Self::Text => {
                if text
                    .chars()
                    .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
                {
                    return Err(
                        i18n_key::REDIS_THIS_TEXT_CONTAINS_CONTROL_CHARACTERS_USE_ESCAPED_TEXT_TO_PRESERVE,
                    );
                }
                text.to_owned()
            }
            Self::Escaped => escaped(bytes)
                .ok_or(i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR)?,
        };
        if encoded.len() > EDIT_BYTES {
            return Err(i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR);
        }
        Ok(encoded)
    }

    pub(super) fn parse(self, text: &str) -> Result<Vec<u8>, &'static str> {
        if text.len() > EDIT_BYTES {
            return Err(i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR);
        }
        match self {
            Self::Text => Ok(text.as_bytes().to_vec()),
            Self::Escaped => parse(text),
        }
    }
}

pub(super) fn partial_string(
    info: &rustterm::redis::KeyInfo,
    page: &rustterm::redis::PageRequest,
) -> bool {
    matches!(&info.data, rustterm::redis::KeyData::String(bytes)
        if page.offset != 0 || info.next.is_some() || info.length != bytes.len() as u64)
}

pub(super) fn label(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "\"\"".into();
    }
    let mut output = String::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        let mut chars = text.chars();
        for ch in chars.by_ref().take(180) {
            output.extend(ch.escape_debug());
        }
        if chars.next().is_some() {
            output.push('…');
        }
    } else {
        use std::fmt::Write as _;
        output.push_str("0x");
        for byte in bytes.iter().take(80) {
            let _ = write!(output, "{byte:02x}");
        }
        if bytes.len() > 80 {
            output.push('…');
        }
    }
    output
}

pub(super) fn preview(bytes: &[u8]) -> String {
    preview_limit(bytes, PREVIEW_BYTES)
}

pub(super) fn full_preview(bytes: &[u8]) -> String {
    preview_limit(bytes, 64 * 1024)
}

fn preview_limit(bytes: &[u8], limit: usize) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let mut end = text.len().min(limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = &text[..end];
        // Do not reserialize JSON here: parsing its numbers as floats could
        // silently change their precision in the displayed value.
        let mut result = String::with_capacity(text.len());
        for ch in text.chars() {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                result.extend(ch.escape_debug());
            } else {
                result.push(ch);
            }
        }
        if end < bytes.len() {
            result.push_str("\n…");
        }
        result
    } else {
        use std::fmt::Write as _;
        let mut result = String::new();
        for (index, chunk) in bytes.chunks(16).take(limit / 16).enumerate() {
            let _ = write!(result, "{:08x}  ", index * 16);
            for byte in chunk {
                let _ = write!(result, "{byte:02x} ");
            }
            result.push('\n');
        }
        if bytes.len() > limit {
            result.push('…');
        }
        result
    }
}

pub(super) fn editable(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    (text.len() <= EDIT_BYTES).then(|| text.to_owned())
}

fn escaped(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let json = serde_json::to_string(text).ok()?;
    // JSON allows literal DEL and C1 controls, but the shared single-line input
    // strips those characters. Escape them before assigning the draft as well.
    let mut encoded = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch.is_control() {
            use std::fmt::Write as _;
            let _ = write!(encoded, "\\u{:04x}", u32::from(ch));
        } else {
            encoded.push(ch);
        }
    }
    (encoded.len() <= EDIT_BYTES).then_some(encoded)
}

pub(super) fn parse(text: &str) -> Result<Vec<u8>, &'static str> {
    serde_json::from_str::<String>(text)
        .map(String::into_bytes)
        .map_err(|_| i18n_key::REDIS_ENTER_A_JSON_STRING_INCLUDING_QUOTES_USE_N_FOR_A)
}

/// Split once when a Redis reply arrives. The renderer only clones visible lines.
#[derive(Default)]
pub(super) struct Preview {
    pub lines: Vec<gpui::SharedString>,
    pub widest: usize,
}

impl Preview {
    pub fn new(text: String) -> Self {
        let lines: Vec<gpui::SharedString> = text
            .split('\n')
            .map(|line| line.to_owned().into())
            .collect();
        let widest = lines
            .iter()
            .enumerate()
            .max_by_key(|(_, line)| {
                line.chars()
                    .map(|ch| {
                        if ch == '\t' {
                            4
                        } else if ch.is_ascii() {
                            1
                        } else {
                            2
                        }
                    })
                    .sum::<usize>()
            })
            .map_or(0, |(index, _)| index);
        Self { lines, widest }
    }
}

/// Validate without materializing numbers, then add only insignificant whitespace.
/// This preserves numeric precision, duplicate object keys and string escapes.
/// Both limits bound formatting work for remote, potentially hostile values.
pub(super) fn json_preview(info: &super::KeyInfo, page: &super::PageRequest) -> Option<String> {
    let super::KeyData::String(bytes) = &info.data else {
        return None;
    };
    if page.offset != 0 || info.next.is_some() || info.length != bytes.len() as u64 {
        return None;
    }
    pretty_json(bytes)
}

fn pretty_json(bytes: &[u8]) -> Option<String> {
    const MAX_INPUT: usize = 64 * 1024;
    const MAX_OUTPUT: usize = 256 * 1024;
    if bytes.len() > MAX_INPUT {
        return None;
    }
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).ok()?;
    let text = std::str::from_utf8(bytes).ok()?;
    let mut output = String::with_capacity(bytes.len());
    let mut chars = text.chars().peekable();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut previous = '\0';
    let mut lines = 1;
    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => {
                    output.push(ch);
                    in_string = true;
                }
                '{' | '[' => {
                    output.push(ch);
                    depth += 1;
                    while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                        chars.next();
                    }
                    if !chars.peek().is_some_and(|c| matches!(c, '}' | ']')) {
                        output.push('\n');
                        output.push_str(&"  ".repeat(depth));
                        lines += 1;
                    }
                }
                '}' | ']' => {
                    depth = depth.saturating_sub(1);
                    if !matches!(previous, '{' | '[') {
                        output.push('\n');
                        output.push_str(&"  ".repeat(depth));
                        lines += 1;
                    }
                    output.push(ch);
                }
                ',' => {
                    output.push_str(",\n");
                    output.push_str(&"  ".repeat(depth));
                    lines += 1;
                }
                ':' => output.push_str(": "),
                ch if ch.is_ascii_whitespace() => continue,
                _ => output.push(ch),
            }
        }
        previous = ch;
        if output.len() > MAX_OUTPUT || lines > 4096 {
            return None;
        }
    }
    Some(output)
}

pub(super) fn encoding(data: &super::KeyData) -> Option<&'static str> {
    let is_text = |bytes: &[u8]| std::str::from_utf8(bytes).is_ok();
    let text = match data {
        super::KeyData::String(bytes) => is_text(bytes),
        super::KeyData::Hash(rows) | super::KeyData::ZSet(rows) => rows
            .iter()
            .all(|(key, value)| is_text(key) && is_text(value)),
        super::KeyData::List(rows) | super::KeyData::Set(rows) => {
            rows.iter().all(|value| is_text(value))
        }
        super::KeyData::Stream(rows) => rows.iter().all(|row| {
            row.fields
                .iter()
                .all(|(key, value)| is_text(key) && is_text(value))
        }),
        _ => return None,
    };
    Some(if text {
        i18n_key::REDIS_UTF_8
    } else {
        i18n_key::REDIS_BINARY
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_formatting_preserves_tokens_and_bounds() {
        let source = br#"{"n":184467440737095516161,"n":1.2300e+40,"s":"a\\b\"c","empty":[]}"#;
        let formatted = pretty_json(source).unwrap();
        assert!(formatted.contains("184467440737095516161"));
        assert!(formatted.contains("1.2300e+40"));
        assert_eq!(formatted.matches("\"n\"").count(), 2);
        assert!(formatted.contains(r#""s": "a\\b\"c""#));
        assert!(formatted.contains("\"empty\": []"));
        assert!(pretty_json(b"not json").is_none());
        assert!(pretty_json(&vec![b' '; 65537]).is_none());
        assert!(pretty_json(&[0xff]).is_none());
    }

    #[test]
    fn editor_preserves_controls_and_unicode_without_loss() {
        let data = "中\n\r\t\0\"\\\u{7f}\u{85}\u{9f}".as_bytes();
        let encoded = escaped(data).unwrap();
        assert!(!encoded.chars().any(char::is_control));
        assert_eq!(parse(&encoded).unwrap(), data);
        assert!(editable(&[0xff, 0xfe]).is_none());
        assert!(escaped(&vec![b'\n'; EDIT_BYTES]).is_none());
        assert!(editable(&vec![b'\n'; EDIT_BYTES]).is_some());
        assert!(parse("unquoted").is_err());
    }

    #[test]
    fn text_editor_treats_quotes_backslashes_and_newlines_as_literal_data() {
        let bytes = " {\"message\": \"中文\\n\\path\"}\n\t next line ".as_bytes();
        let text = EditEncoding::Text.encode(bytes, true).unwrap();
        assert_eq!(EditEncoding::Text.parse(&text).unwrap(), bytes);
        let escaped = EditEncoding::Escaped.encode(bytes, true).unwrap();
        assert_eq!(EditEncoding::Escaped.parse(&escaped).unwrap(), bytes);
        assert!(EditEncoding::Text.encode(bytes, false).is_err());
        for bytes in [b"a\r\nb".as_slice(), b"a\0b".as_slice()] {
            assert!(EditEncoding::Text.encode(bytes, true).is_err());
            let escaped = EditEncoding::Escaped.encode(bytes, true).unwrap();
            assert_eq!(EditEncoding::Escaped.parse(&escaped).unwrap(), bytes);
        }
        assert!(EditEncoding::Escaped.parse("unquoted").is_err());
    }

    #[test]
    fn previews_are_bounded_at_utf8_boundaries() {
        let large = "中".repeat(PREVIEW_BYTES);
        assert!(preview(large.as_bytes()).len() < PREVIEW_BYTES + 8);
        assert_eq!(label(b"a\nb"), "a\\nb");
        assert_eq!(label(&[0xff, 0]), "0xff00");
        assert_eq!(preview(b"184467440737095516161"), "184467440737095516161");
    }

    #[test]
    fn copy_scope_distinguishes_complete_first_last_and_changed_string_pages() {
        use rustterm::redis::{KeyData, KeyInfo, KeyType, PageRequest};
        let mut info = KeyInfo {
            kind: KeyType::String,
            ttl_ms: -1,
            length: 3,
            data: KeyData::String(b"abc".to_vec()),
            next: None,
        };
        assert!(!partial_string(&info, &PageRequest::default()));
        info.next = Some(PageRequest {
            offset: 3,
            ..Default::default()
        });
        assert!(partial_string(&info, &PageRequest::default()));
        info.next = None;
        assert!(partial_string(
            &info,
            &PageRequest {
                offset: 3,
                ..Default::default()
            }
        ));
        info.length = 6;
        assert!(partial_string(&info, &PageRequest::default()));
        info.data = KeyData::List(vec![b"abc".to_vec()]);
        assert!(!partial_string(&info, &PageRequest::default()));
    }
}
