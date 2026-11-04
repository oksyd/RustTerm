//! Shared lexical boundaries for SQL editing and session state tracking.
use crate::i18n::messages as i18n_key;

use crate::config::SqlEngine;
use std::ops::Range;

pub const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCK",
    "MATERIALIZED",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECEDING",
    "PRIMARY",
    "PROCEDURE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Identifier,
    String,
    Number,
    Comment,
    Operator,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: Kind,
    pub complete: bool,
}

fn word_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}
fn word_continue(ch: char) -> bool {
    word_start(ch) || ch.is_numeric() || ch == '$'
}

pub fn lex(text: &str, engine: SqlEngine) -> Vec<Token> {
    lex_with_rules(text, engine, rules(engine)[0])
}

#[derive(Clone, Copy)]
struct QuoteRules {
    backslash_strings: bool,
    double_quoted_identifiers: bool,
}

fn rules(engine: SqlEngine) -> &'static [QuoteRules] {
    match engine {
        SqlEngine::Mysql => &[
            QuoteRules {
                backslash_strings: true,
                double_quoted_identifiers: false,
            },
            QuoteRules {
                backslash_strings: false,
                double_quoted_identifiers: false,
            },
            QuoteRules {
                backslash_strings: true,
                double_quoted_identifiers: true,
            },
            QuoteRules {
                backslash_strings: false,
                double_quoted_identifiers: true,
            },
        ],
        SqlEngine::Postgres => &[
            QuoteRules {
                backslash_strings: false,
                double_quoted_identifiers: true,
            },
            QuoteRules {
                backslash_strings: true,
                double_quoted_identifiers: true,
            },
        ],
        SqlEngine::Sqlite => &[QuoteRules {
            backslash_strings: false,
            double_quoted_identifiers: true,
        }],
    }
}

fn lex_with_rules(text: &str, engine: SqlEngine, rules: QuoteRules) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let ch = text[i..].chars().next().unwrap();
        let mut complete = true;
        let kind;
        if ch.is_whitespace() {
            i += ch.len_utf8();
            while i < bytes.len() && text[i..].chars().next().unwrap().is_whitespace() {
                i += text[i..].chars().next().unwrap().len_utf8();
            }
            kind = Kind::Plain;
        } else if bytes[i..].starts_with(b"--")
            && (engine != SqlEngine::Mysql || bytes.get(i + 2).is_none_or(u8::is_ascii_whitespace))
            || (engine == SqlEngine::Mysql && ch == '#')
        {
            i = text[i..]
                .find('\n')
                .map_or(bytes.len(), |offset| i + offset);
            kind = Kind::Comment;
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if engine == SqlEngine::Postgres && bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += text[i..].chars().next().unwrap().len_utf8();
                }
            }
            complete = depth == 0;
            kind = Kind::Comment;
        } else if engine == SqlEngine::Postgres
            && ch == '$'
            && let Some(end) = dollar_tag(&text[i..])
        {
            let tag = &text[i..i + end];
            let body = i + end;
            if let Some(offset) = text[body..].find(tag) {
                i = body + offset + end;
            } else {
                i = bytes.len();
                complete = false;
            }
            kind = Kind::String;
        } else if matches!(ch, '\'' | '"')
            || (ch == '`' && engine != SqlEngine::Postgres)
            || (ch == '[' && engine == SqlEngine::Sqlite)
        {
            let close = if ch == '[' { ']' } else { ch };
            let string = ch == '\'' || (ch == '"' && !rules.double_quoted_identifiers);
            let escaped = string
                && (rules.backslash_strings
                    || (engine == SqlEngine::Postgres
                        && ch == '\''
                        && start > 0
                        && matches!(bytes[start - 1], b'e' | b'E')
                        && (start < 2
                            || !word_continue(text[..start - 1].chars().next_back().unwrap()))));
            i += ch.len_utf8();
            complete = false;
            while i < bytes.len() {
                let next = text[i..].chars().next().unwrap();
                i += next.len_utf8();
                if escaped && next == '\\' {
                    if i < bytes.len() {
                        i += text[i..].chars().next().unwrap().len_utf8();
                    }
                } else if next == close {
                    if ch != '[' && text[i..].starts_with(close) {
                        i += close.len_utf8();
                    } else {
                        complete = true;
                        break;
                    }
                }
            }
            kind = if string {
                Kind::String
            } else {
                Kind::Identifier
            };
        } else if word_start(ch) {
            i += ch.len_utf8();
            while i < bytes.len() && word_continue(text[i..].chars().next().unwrap()) {
                i += text[i..].chars().next().unwrap().len_utf8();
            }
            kind = if KEYWORDS
                .iter()
                .any(|word| text[start..i].eq_ignore_ascii_case(word))
            {
                Kind::Keyword
            } else {
                Kind::Identifier
            };
        } else if ch.is_ascii_digit() {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_'))
            {
                i += 1;
            }
            kind = Kind::Number;
        } else {
            i += ch.len_utf8();
            kind = Kind::Operator;
        }
        tokens.push(Token {
            range: start..i,
            kind,
            complete,
        });
    }
    tokens
}

fn dollar_tag(text: &str) -> Option<usize> {
    let end = text[1..].find('$')? + 1;
    let tag = &text[1..end];
    if tag.is_empty()
        || tag.chars().next().is_some_and(word_start)
            && tag.chars().all(|ch| word_start(ch) || ch.is_numeric())
    {
        Some(end + 1)
    } else {
        None
    }
}

pub fn statement_range(
    text: &str,
    cursor: usize,
    engine: SqlEngine,
) -> Result<Range<usize>, &'static str> {
    let variants = rules(engine);
    let baseline = statements_with_rules(text, engine, variants[0])
        .and_then(|statements| statement_at(&statements, cursor));
    if text.contains('\\') {
        for variant in &variants[1..] {
            let alternative = statements_with_rules(text, engine, *variant)
                .and_then(|statements| statement_at(&statements, cursor));
            if alternative != baseline {
                return Err(
                    i18n_key::DATABASE_SQL_QUOTING_DEPENDS_ON_SESSION_SETTINGS_SELECT_THE_STATEMENT_EXPLICITLY,
                );
            }
        }
    }
    baseline
}

fn statement_at(statements: &[Statement], cursor: usize) -> Result<Range<usize>, &'static str> {
    let selected = statements
        .iter()
        .find(|statement| statement.range.start <= cursor && cursor < statement.range.end)
        .or_else(|| {
            statements
                .iter()
                .rev()
                .find(|statement| statement.range.end <= cursor)
        })
        .or_else(|| statements.first());
    match selected {
        Some(statement) if !statement.complete => {
            Err(i18n_key::DATABASE_SQL_CONTAINS_AN_UNFINISHED_STRING_OR_COMMENT)
        }
        Some(statement) => Ok(statement.range.clone()),
        None => Err(i18n_key::DATABASE_NO_SQL_STATEMENT_AT_THE_CURSOR),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub range: Range<usize>,
    pub complete: bool,
}

pub fn statements(text: &str, engine: SqlEngine) -> Result<Vec<Statement>, &'static str> {
    let variants = rules(engine);
    let baseline = statements_with_rules(text, engine, variants[0])?;
    if text.contains('\\') {
        for variant in &variants[1..] {
            if statements_with_rules(text, engine, *variant).as_ref() != Ok(&baseline) {
                return Err(
                    i18n_key::DATABASE_SQL_QUOTING_DEPENDS_ON_SESSION_SETTINGS_SELECT_THE_STATEMENT_EXPLICITLY,
                );
            }
        }
    }
    Ok(baseline)
}

fn statements_with_rules(
    text: &str,
    engine: SqlEngine,
    rules: QuoteRules,
) -> Result<Vec<Statement>, &'static str> {
    let tokens = lex_with_rules(text, engine, rules);
    let mut statements = Vec::new();
    let mut start = None;
    let mut end = 0;
    let mut words = Vec::new();
    let mut compound = false;
    let mut depth = 0_usize;
    let mut cases = 0_usize;
    let mut incomplete = false;
    let mut end_qualifier = None;
    for (index, token) in tokens.iter().enumerate() {
        let value = &text[token.range.clone()];
        if token.kind == Kind::Plain
            || token.kind == Kind::Comment
                && !(engine == SqlEngine::Mysql && value.starts_with("/*!"))
        {
            if !token.complete {
                incomplete = true;
            }
            continue;
        }
        if value == ";" && start.is_none() {
            continue;
        }
        start.get_or_insert(token.range.start);
        end = token.range.end;
        incomplete |= !token.complete;
        if matches!(token.kind, Kind::Keyword | Kind::Identifier)
            && !value.starts_with(['"', '`', '['])
        {
            if words.len() < 8 {
                words.push(value.to_ascii_uppercase());
            }
            if words.first().is_some_and(|word| word == "DELIMITER") {
                return Err(i18n_key::DATABASE_CLIENT_DELIMITER_DIRECTIVES_ARE_NOT_SUPPORTED);
            }
            if words.first().is_some_and(|word| word == "CREATE")
                && matches!(
                    value.to_ascii_uppercase().as_str(),
                    "TRIGGER" | "PROCEDURE" | "FUNCTION" | "EVENT"
                )
                && engine != SqlEngine::Postgres
            {
                compound = true;
            }
            if compound {
                if end_qualifier == Some(index) {
                    end_qualifier = None;
                } else if value.eq_ignore_ascii_case("BEGIN") {
                    depth += 1;
                } else if value.eq_ignore_ascii_case("CASE") {
                    cases += 1;
                } else if value.eq_ignore_ascii_case("END") {
                    let qualifier = tokens
                        .iter()
                        .enumerate()
                        .skip(index + 1)
                        .find(|(_, token)| !matches!(token.kind, Kind::Plain | Kind::Comment))
                        .filter(|(_, token)| {
                            matches!(
                                text[token.range.clone()].to_ascii_uppercase().as_str(),
                                "IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT"
                            )
                        });
                    if let Some((next, token)) = qualifier {
                        end_qualifier = Some(next);
                        if text[token.range.clone()].eq_ignore_ascii_case("CASE") {
                            cases = cases.saturating_sub(1);
                        }
                    } else if cases > 0 {
                        cases -= 1;
                    } else {
                        depth = depth.saturating_sub(1);
                    }
                }
            }
        }
        if value == ";" && depth == 0 {
            if let Some(start) = start.take() {
                statements.push(Statement {
                    range: start..end,
                    complete: !incomplete,
                });
            }
            words.clear();
            compound = false;
            cases = 0;
            incomplete = false;
            end_qualifier = None;
        }
    }
    if let Some(start) = start {
        statements.push(Statement {
            range: start..end,
            complete: !incomplete && depth == 0,
        });
    }
    Ok(statements)
}

pub fn leading_words(text: &str, engine: SqlEngine) -> Vec<String> {
    lex(text, engine)
        .iter()
        .filter(|token| !matches!(token.kind, Kind::Plain | Kind::Comment))
        .take_while(|token| {
            matches!(token.kind, Kind::Keyword | Kind::Identifier)
                && !text[token.range.clone()].starts_with(['"', '`', '['])
        })
        .take(4)
        .map(|token| text[token.range.clone()].to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, needle: &str, engine: SqlEngine) -> String {
        text[statement_range(text, text.find(needle).unwrap(), engine).unwrap()].to_owned()
    }

    #[test]
    fn postgres_strings_and_nested_comments_do_not_split_statements() {
        let sql = "SELECT 'a;''b'; /* outer; /* inner; */ */ DO $body$ BEGIN RAISE NOTICE ';'; END $body$; SELECT 2;";
        assert_eq!(
            at(sql, "NOTICE", SqlEngine::Postgres),
            "DO $body$ BEGIN RAISE NOTICE ';'; END $body$;"
        );
        assert_eq!(at(sql, "SELECT 2", SqlEngine::Postgres), "SELECT 2;");
        assert_eq!(
            at("SELECT E'a\\';b'; SELECT 2;", "b'", SqlEngine::Postgres),
            "SELECT E'a\\';b';"
        );
    }

    #[test]
    fn mysql_comments_and_backslash_strings_are_dialect_aware() {
        let sql = "SELECT 'a;''b'; # comment;\nSELECT 2--1; SELECT 3;";
        assert_eq!(at(sql, "b'", SqlEngine::Mysql), "SELECT 'a;''b';");
        assert_eq!(at(sql, "2--1", SqlEngine::Mysql), "SELECT 2--1;");
        assert!(statement_range("DELIMITER //\nSELECT 1//", 0, SqlEngine::Mysql).is_err());
    }

    #[test]
    fn session_dependent_quoting_requires_explicit_execution_boundaries() {
        let sql = "SELECT 'a\\'; SELECT 2; -- '; SELECT 3;";
        for engine in [SqlEngine::Mysql, SqlEngine::Postgres] {
            assert!(
                statement_range(sql, 0, engine)
                    .unwrap_err()
                    .starts_with("SQL quoting depends")
            );
            assert!(statements(sql, engine).is_err());
        }
        let ansi = "SELECT \"a\\\"; SELECT 2; -- \"; SELECT 3;";
        assert!(
            statement_range(ansi, 0, SqlEngine::Mysql)
                .unwrap_err()
                .starts_with("SQL quoting depends")
        );
        // A later ambiguous statement does not disable an unambiguous current one.
        let prefixed = format!("SELECT 0; {sql}");
        assert_eq!(
            &prefixed[statement_range(&prefixed, 0, SqlEngine::Mysql).unwrap()],
            "SELECT 0;"
        );
        // Explicit escape literals and backtick identifiers have fixed rules.
        assert_eq!(
            at("SELECT E'a\\';b'; SELECT 2;", "b'", SqlEngine::Postgres),
            "SELECT E'a\\';b';"
        );
        assert_eq!(
            at("SELECT `a\\`; SELECT 2;", "a\\", SqlEngine::Mysql),
            "SELECT `a\\`;"
        );
    }

    #[test]
    fn sqlite_trigger_is_a_single_statement() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES(CASE WHEN 1 THEN 2 ELSE 3 END); UPDATE y SET z=4; END; SELECT 5;";
        assert!(at(sql, "UPDATE", SqlEngine::Sqlite).starts_with("CREATE TRIGGER"));
        assert_eq!(at(sql, "SELECT 5", SqlEngine::Sqlite), "SELECT 5;");
    }

    #[test]
    fn mysql_compound_end_qualifiers_and_executable_comments_are_preserved() {
        let sql = "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; CASE 1 WHEN 1 THEN SELECT 2; END CASE; SELECT 3; END; SELECT 4;";
        assert!(at(sql, "SELECT 3", SqlEngine::Mysql).starts_with("CREATE PROCEDURE"));
        assert_eq!(at(sql, "SELECT 4", SqlEngine::Mysql), "SELECT 4;");
        assert_eq!(
            at(
                "/*! SET autocommit=0 */; SELECT 1;",
                "autocommit",
                SqlEngine::Mysql
            ),
            "/*! SET autocommit=0 */;"
        );
    }

    #[test]
    fn cursor_after_semicolon_does_not_execute_the_following_statement() {
        let sql = "SELECT 1;\n\nDELETE FROM users;";
        assert_eq!(
            &sql[statement_range(sql, 9, SqlEngine::Postgres).unwrap()],
            "SELECT 1;"
        );
        assert_eq!(
            &sql[statement_range(sql, 10, SqlEngine::Postgres).unwrap()],
            "SELECT 1;"
        );
        assert_eq!(at(sql, "DELETE", SqlEngine::Postgres), "DELETE FROM users;");
        assert!(statement_range(" ; ; ", 0, SqlEngine::Sqlite).is_err());
    }

    #[test]
    fn unicode_and_incomplete_statements_keep_safe_boundaries() {
        let sql = "SELECT '中;文';\nSELECT \"名称\" FROM \"表\";";
        assert_eq!(
            at(sql, "名称", SqlEngine::Postgres),
            "SELECT \"名称\" FROM \"表\";"
        );
        assert!(statement_range("SELECT 'unfinished", 0, SqlEngine::Postgres).is_err());
        assert!(statement_range("SELECT 1 /* unfinished", 0, SqlEngine::Postgres).is_err());
        assert!(statement_range("-- only a comment", 0, SqlEngine::Postgres).is_err());
        for token in lex(sql, SqlEngine::Postgres) {
            assert!(
                sql.is_char_boundary(token.range.start) && sql.is_char_boundary(token.range.end)
            );
        }
    }
}
