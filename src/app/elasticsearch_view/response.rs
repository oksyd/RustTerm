use gpui::SharedString;
use rustterm::elasticsearch::{ElasticsearchError, Response, Result, format_json};
use std::sync::Arc;

pub(super) struct PreparedResponse {
    pub status: (u16, u128),
    pub json: Arc<String>,
    pub lines: Vec<SharedString>,
}
impl PreparedResponse {
    pub fn new(response: Response) -> Result<Self> {
        let json = format_json(&response.json)?;
        let lines = display_lines(&json)?;
        Ok(Self {
            status: (response.status, response.elapsed.as_millis()),
            json: Arc::new(json),
            lines,
        })
    }
}

fn display_lines(json: &str) -> Result<Vec<SharedString>> {
    let mut lines = Vec::new();
    for mut line in json.lines() {
        // A single log message must not turn into a multi-megabyte shaped line.
        // Copy JSON retains the original document; only display rows are wrapped.
        loop {
            if lines.len() >= 100_000 {
                return Err(ElasticsearchError::TooLarge);
            }
            let mut end = line.len().min(4096);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            lines.push(line[..end].to_owned().into());
            line = &line[end..];
            if line.is_empty() {
                break;
            }
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_unicode_values_wrap_without_changing_copyable_json() {
        let value = serde_json::json!({"message":"中😀".repeat(3000)});
        let response = PreparedResponse::new(Response {
            status: 200,
            elapsed: std::time::Duration::ZERO,
            json: value.clone(),
        })
        .unwrap();
        assert!(response.lines.iter().all(|line| line.len() <= 4096));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response.json).unwrap(),
            value
        );
        assert!(response.lines.len() > 4);
    }
    #[test]
    fn many_tiny_rows_have_a_bounded_display_cost() {
        assert!(display_lines(&"0\n".repeat(100_001)).is_err());
    }
}
