use super::{super::Theme, Span};
use regex::Regex;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    ops::Range,
    sync::LazyLock,
};

struct Rules {
    url: Regex,
    email: Regex,
    path: Regex,
    timestamp: Regex,
    ipv6: Regex,
    mac: Regex,
    ipv4: Regex,
    word: Regex,
    option: Regex,
    hex: Regex,
    identifier: Regex,
}

static RULES: LazyLock<Rules> = LazyLock::new(|| {
    let compile = |pattern| Regex::new(pattern).expect("built-in highlight pattern");
    Rules {
        url: compile(r#"(?i:(?:https?|ftps?|ssh|sftp|file)://|mailto:)[^\s<>"'，。；！？]+"#),
        email: compile(
            r"[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,63}",
        ),
        path: compile(r#"(?:[A-Za-z]:[\\/]|\.{1,2}/|~/|/|\\\\)[^\s<>"'，。；！？]+"#),
        timestamp: compile(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}(?:[T ][0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:?[0-9]{2})?)?|[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?",
        ),
        ipv6: compile(r"[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]+(?:%[A-Za-z0-9_.-]+)?"),
        mac: compile(r"(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}|(?:[0-9A-Fa-f]{2}-){5}[0-9A-Fa-f]{2}"),
        ipv4: compile(r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}"),
        word: compile(r"\w+(?:['’]\w+)*"),
        option: compile(r"--?[A-Za-z][A-Za-z0-9_-]*"),
        hex: compile(r"(?i:(?:0x)?(?:[0-9a-f]{8}|[0-9a-f]{4}))"),
        identifier: compile(r"\w"),
    }
});

/// Rules claim complete tokens in priority order. The byte map makes overlap
/// checks bounded and produces sorted spans without comparing every span pair.
pub(super) fn spans(text: &str, theme: &Theme) -> Vec<Span> {
    let rules = &*RULES;
    let mut colors = vec![None; text.len()];

    for found in rules.url.find_iter(text) {
        let range = trimmed_token(text, found.range(), false);
        if left_boundary(text, range.start, "./\\@-") && valid_url(&text[range.clone()]) {
            paint(&mut colors, range, theme.url);
        }
    }
    for found in rules.email.find_iter(text) {
        if token_boundary(text, &found.range(), "@/\\-", "@/\\-") && valid_email(found.as_str()) {
            paint(&mut colors, found.range(), theme.url);
        }
    }
    for found in rules.path.find_iter(text) {
        let range = trimmed_token(text, found.range(), true);
        if range.len() > 1 && left_boundary(text, range.start, "./\\@-") {
            paint(&mut colors, range, theme.path);
        }
    }
    for found in rules.timestamp.find_iter(text) {
        if token_boundary(text, &found.range(), ":/\\@-", ":/\\@-")
            && valid_timestamp(found.as_str())
        {
            paint(&mut colors, found.range(), theme.timestamp);
        }
    }
    for found in rules.ipv6.find_iter(text) {
        if token_boundary(text, &found.range(), ":/\\@%+-", ":/\\@%+-")
            && valid_ipv6(found.as_str())
        {
            paint(&mut colors, found.range(), theme.address);
        }
    }
    for found in rules.mac.find_iter(text) {
        if token_boundary(text, &found.range(), ":/\\@%-", ":/\\@%-") {
            paint(&mut colors, found.range(), theme.address);
        }
    }
    for found in rules.ipv4.find_iter(text) {
        if token_boundary(text, &found.range(), ":/\\@%-", "/\\@%-")
            && found.as_str().parse::<Ipv4Addr>().is_ok()
        {
            paint(&mut colors, found.range(), theme.address);
        }
    }

    let words: Vec<_> = rules.word.find_iter(text).collect();
    for (index, word) in words.iter().enumerate() {
        if !is_negation(word.as_str()) || !word_boundary(text, &word.range()) {
            continue;
        }
        paint(&mut colors, word.range(), theme.error);
        if let Some(next) = words.get(index + 1) {
            let gap = &text[word.end()..next.start()];
            if !gap.is_empty()
                && gap.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
                && word_boundary(text, &next.range())
            {
                paint(&mut colors, word.end()..next.end(), theme.error);
            }
        }
    }
    for (index, word) in words.iter().enumerate() {
        if !word_boundary(text, &word.range()) {
            continue;
        }
        if word.as_str().eq_ignore_ascii_case("starting") && index > 0 {
            let label = &words[index - 1];
            if label.as_str().eq_ignore_ascii_case("health")
                && word_boundary(text, &label.range())
                && text[label.end()..word.start()].trim_matches([' ', '\t']) == ":"
            {
                paint(&mut colors, label.start()..word.end(), theme.warn);
            }
            continue;
        }
        let color = match word.as_str() {
            "TRACE" => theme.trace,
            "DEBUG" => theme.debug,
            "INFO" => theme.info,
            "WARN" => theme.warn,
            "ERROR" => theme.error,
            "FATAL" => theme.fatal,
            value
                if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("healthy") =>
            {
                theme.info
            }
            value
                if value.eq_ignore_ascii_case("false")
                    || value.eq_ignore_ascii_case("unhealthy") =>
            {
                theme.error
            }
            _ => continue,
        };
        paint(&mut colors, word.range(), color);
    }
    for found in rules.option.find_iter(text) {
        if word_boundary(text, &found.range()) {
            paint(&mut colors, found.range(), theme.debug);
        }
    }
    for found in rules.hex.find_iter(text) {
        let value = found.as_str();
        let prefixed = value.starts_with("0x") || value.starts_with("0X");
        if token_boundary(text, &found.range(), ":/\\@-", ":/\\@-")
            && (prefixed || value.len() == 8 || value.bytes().any(|byte| byte.is_ascii_digit()))
        {
            paint(&mut colors, found.range(), theme.warn);
        }
    }

    let mut spans = Vec::new();
    let mut start = 0;
    while start < colors.len() {
        let Some(color) = colors[start] else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < colors.len() && colors[end] == Some(color) {
            end += 1;
        }
        spans.push(Span {
            range: start..end,
            color,
        });
        start = end;
    }
    spans
}

fn paint(colors: &mut [Option<u32>], range: Range<usize>, color: u32) {
    if !range.is_empty() && colors[range.clone()].iter().all(Option::is_none) {
        colors[range].fill(Some(color));
    }
}

fn identifier(character: char) -> bool {
    RULES
        .identifier
        .is_match(character.encode_utf8(&mut [0; 4]))
}

fn left_boundary(text: &str, start: usize, punctuation: &str) -> bool {
    text[..start].chars().next_back().is_none_or(|character| {
        !identifier(character) && character != '.' && !punctuation.contains(character)
    })
}

fn token_boundary(text: &str, range: &Range<usize>, left: &str, right: &str) -> bool {
    left_boundary(text, range.start, left) && {
        let mut following = text[range.end..].chars();
        following.next().is_none_or(|character| {
            !identifier(character)
                && !right.contains(character)
                && (character != '.'
                    || following
                        .next()
                        .is_none_or(|next| next.is_whitespace() || ")]}'\"".contains(next)))
        })
    }
}

fn word_boundary(text: &str, range: &Range<usize>) -> bool {
    token_boundary(text, range, "/\\@-'’", "/\\@-'’")
}

/// Keep balanced delimiters inside a URL/path, leaving surrounding punctuation
/// for the terminal's bracket decoration. An unmatched closer also ends a token
/// in compact JSON such as ["/tmp/a"], rather than swallowing the next value.
fn trimmed_token(text: &str, range: Range<usize>, path: bool) -> Range<usize> {
    let mut stack = Vec::new();
    let mut end = range.end;
    for (offset, character) in text[range.clone()].char_indices() {
        match character {
            '(' | '[' | '{' => stack.push(character),
            ')' | ']' | '}' => {
                let opening = match character {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(opening) {
                    end = range.start + offset;
                    break;
                }
            }
            ',' | ';' if path => {
                end = range.start + offset;
                break;
            }
            _ => {}
        }
    }
    while end > range.start
        && matches!(
            text.as_bytes()[end - 1],
            b'.' | b',' | b';' | b'!' | b'?' | b':'
        )
    {
        end -= 1;
    }
    range.start..end
}

fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && domain
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
}

fn valid_url(value: &str) -> bool {
    if value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
    {
        let address = value[7..].split('?').next().unwrap_or_default();
        return valid_email(address)
            && RULES
                .email
                .find(address)
                .is_some_and(|found| found.range() == (0..address.len()));
    }
    value
        .split_once("://")
        .is_some_and(|(_, target)| !target.is_empty())
}

fn valid_ipv6(value: &str) -> bool {
    let (address, zone) = value
        .split_once('%')
        .map_or((value, None), |(address, zone)| (address, Some(zone)));
    address.parse::<Ipv6Addr>().is_ok()
        && zone.is_none_or(|zone| {
            !zone.is_empty()
                && zone.len() <= 64
                && zone
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        })
}

fn valid_timestamp(value: &str) -> bool {
    let number = |part: &str| part.parse::<u32>().ok();
    let time = if value.as_bytes().get(4) == Some(&b'-') {
        let (Some(year), Some(month), Some(day)) = (
            number(&value[..4]),
            number(&value[5..7]),
            number(&value[8..10]),
        ) else {
            return false;
        };
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if year.is_multiple_of(400)
                || (year.is_multiple_of(4) && !year.is_multiple_of(100)) =>
            {
                29
            }
            2 => 28,
            _ => return false,
        };
        if year == 0 || day == 0 || day > days {
            return false;
        }
        if value.len() == 10 {
            return true;
        }
        &value[11..]
    } else {
        value
    };
    number(&time[..2]).is_some_and(|hour| hour < 24)
        && number(&time[3..5]).is_some_and(|minute| minute < 60)
        && number(&time[6..8]).is_some_and(|second| second <= 60)
}

fn is_negation(value: &str) -> bool {
    [
        "not",
        "don't",
        "doesn't",
        "can't",
        "couldn't",
        "don’t",
        "doesn’t",
        "can’t",
        "couldn’t",
    ]
    .iter()
    .any(|word| value.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(&str, u32)> {
        spans(text, &Theme::default())
            .into_iter()
            .map(|span| (&text[span.range], span.color))
            .collect()
    }

    #[test]
    fn priority_keeps_tokens_whole_and_delimiters_outside() {
        let theme = Theme::default();
        let text = "(https://example.com/a(b)) [/tmp/ERROR], [user@example.com] --output=false";
        assert_eq!(
            tokens(text),
            vec![
                ("https://example.com/a(b)", theme.url),
                ("/tmp/ERROR", theme.path),
                ("user@example.com", theme.url),
                ("--output", theme.debug),
                ("false", theme.error),
            ]
        );
    }

    #[test]
    fn identifiers_and_malformed_addresses_are_not_partial_tokens() {
        assert!(
            tokens("中文true _false falsehood abc--flag x0x1234 face dead /tmp/../falsehood")
                .iter()
                .all(|(_, color)| *color == Theme::default().path)
        );
        assert!(tokens("999.1.2.3 192.0.2.1.5 invalid::1 2001:db8::xyz aa:bb:cc:dd:ee:ff:00 user@@example.com").is_empty());
    }

    #[test]
    fn negation_networks_hex_and_options_use_semantic_colors() {
        let theme = Theme::default();
        assert_eq!(
            tokens(
                "not found TRUE false 0xFACE DEAD1234 DEADBEEF -v fe80::1%eth0 2001:db8::1 00:11:22:aa:bb:cc"
            ),
            vec![
                ("not found", theme.error),
                ("TRUE", theme.info),
                ("false", theme.error),
                ("0xFACE", theme.warn),
                ("DEAD1234", theme.warn),
                ("DEADBEEF", theme.warn),
                ("-v", theme.debug),
                ("fe80::1%eth0", theme.address),
                ("2001:db8::1", theme.address),
                ("00:11:22:aa:bb:cc", theme.address),
            ]
        );
    }

    #[test]
    fn schemes_dates_and_unicode_boundaries_are_explicit() {
        let theme = Theme::default();
        assert_eq!(
            tokens(
                "ftp://host/a ftps://host/a ssh://host sftp://host file:///tmp/a mailto:user@example.com"
            ),
            vec![
                ("ftp://host/a", theme.url),
                ("ftps://host/a", theme.url),
                ("ssh://host", theme.url),
                ("sftp://host", theme.url),
                ("file:///tmp/a", theme.url),
                ("mailto:user@example.com", theme.url),
            ]
        );
        assert_eq!(
            tokens("[2024-02-29T23:59:59Z] 12:34:56.789"),
            vec![
                ("2024-02-29T23:59:59Z", theme.timestamp),
                ("12:34:56.789", theme.timestamp)
            ]
        );
        assert!(tokens("标识符INFO INFO标识符 e\u{301}true --flag中文").is_empty());
    }
    #[test]
    fn network_tokens_do_not_lose_numeric_groups_to_timestamps() {
        let theme = Theme::default();
        assert_eq!(
            tokens("00:11:22:33:44:55 2001:12:30:45::1 192.0.2.1:443 [12:30:45]"),
            vec![
                ("00:11:22:33:44:55", theme.address),
                ("2001:12:30:45::1", theme.address),
                ("192.0.2.1", theme.address),
                ("12:30:45", theme.timestamp),
            ]
        );
    }

    #[test]
    fn health_states_use_complete_words_and_starting_requires_health_context() {
        let theme = Theme::default();
        assert_eq!(
            tokens(
                "Up (healthy) (unhealthy) (health: starting) HEALTHY Unhealthy (Health : Starting)"
            ),
            vec![
                ("healthy", theme.info),
                ("unhealthy", theme.error),
                ("health: starting", theme.warn),
                ("HEALTHY", theme.info),
                ("Unhealthy", theme.error),
                ("Health : Starting", theme.warn),
            ]
        );
        assert_eq!(
            tokens(
                "Up starting restarting healthy-api unhealthy_worker healthiness myhealth: starting health: starting-soon --healthy"
            ),
            vec![("--healthy", theme.debug)],
        );
    }

    #[test]
    fn health_words_do_not_override_negation_or_complete_paths_and_urls() {
        let theme = Theme::default();
        assert_eq!(
            tokens("not healthy /tmp/healthy https://example.com/unhealthy healthy@example.com"),
            vec![
                ("not healthy", theme.error),
                ("/tmp/healthy", theme.path),
                ("https://example.com/unhealthy", theme.url),
                ("healthy@example.com", theme.url),
            ]
        );
    }
}
