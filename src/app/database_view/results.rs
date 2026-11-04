use super::*;
use rustterm::i18n::messages as i18n_key;
use std::{
    cmp::Ordering,
    io::{self, Write},
    path::PathBuf,
};

const COPY_BYTES: usize = 8 * 1024 * 1024;

impl DatabaseView {
    pub(super) fn current_result(&self) -> Option<&ResultSet> {
        self.documents[self.active_document]
            .output
            .as_ref()?
            .results
            .get(self.documents[self.active_document].result_index)
    }

    pub(super) fn filter_rows(&mut self, cx: &mut Context<Self>) {
        self.documents[self.active_document].filter_rows(cx);
        self.resize_drag = None;
        cx.notify();
    }

    pub(super) fn select_result(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.interactive()
            || self.documents[self.active_document]
                .output
                .as_ref()
                .is_none_or(|output| index >= output.results.len())
        {
            return;
        }
        self.documents[self.active_document].result_index = index;
        self.documents[self.active_document].selected_cell = None;
        self.documents[self.active_document].result_revision += 1;
        self.documents[self.active_document].sort = None;
        self.documents[self.active_document]
            .horizontal_scroll
            .set_offset(gpui::Point::default());
        self.filter_rows(cx);
    }

    pub(super) fn sort_rows(&mut self, column: usize, cx: &mut Context<Self>) {
        if self
            .table_workspace()
            .is_some_and(|table| table.section == TableAction::Data)
        {
            self.sort_table(column, cx);
            return;
        }
        if !self.interactive() {
            return;
        }
        self.documents[self.active_document].sort = match self.documents[self.active_document].sort
        {
            Some((old, false)) if old == column => Some((column, true)),
            Some((old, true)) if old == column => None,
            _ => Some((column, false)),
        };
        self.filter_rows(cx);
    }

    pub fn copy_selection(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let input = self.documents[self.active_document].sql_input.read(cx);
        if input.focus_handle().is_focused(window)
            && let Some(text) = input.selected_text()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        } else if self.documents[self.active_document].selected_cell.is_some() {
            self.copy_cell(cx);
        } else {
            self.copy_result(cx);
        }
    }

    pub fn copy_result(&mut self, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some(result) = self.current_result() else {
            return;
        };
        let mut output = Vec::new();
        let mut bounded = LimitedWriter {
            bytes: &mut output,
            limit: COPY_BYTES,
        };
        if write_delimited(
            &mut bounded,
            result,
            &self.documents[self.active_document].visible_rows,
            b'\t',
        )
        .is_err()
        {
            self.error =
                Some(i18n_key::DATABASE_THE_RESULT_IS_TOO_LARGE_FOR_THE_CLIPBOARD_EXPORT_IT.into());
            cx.notify();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            String::from_utf8(output).expect("UTF-8 result values"),
        ));
        self.show_success(
            i18n_key::DATABASE_DISPLAYED_ROWS_COPIED_WITH_COLUMN_NAMES_NULL_IS_WRITTEN_AS,
            cx,
        );
        cx.notify();
    }

    pub(super) fn copy_cell(&mut self, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some((row, column)) = self.documents[self.active_document].selected_cell else {
            return;
        };
        let Some(cell) = self
            .current_result()
            .and_then(|result| result.rows.get(row))
            .and_then(|row| row.get(column))
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(
            cell.as_deref().unwrap_or("\\N").to_owned(),
        ));
        self.show_success(
            i18n_key::DATABASE_SELECTED_CELL_COPIED_NULL_IS_WRITTEN_AS_N,
            cx,
        );
        cx.notify();
    }

    pub(super) fn export_csv(&mut self, cx: &mut Context<Self>) {
        if !self.interactive() || self.current_result().is_none() {
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let output = self.documents[self.active_document]
            .output
            .as_ref()
            .unwrap()
            .clone();
        let index = self.documents[self.active_document].result_index;
        let rows = self.documents[self.active_document].visible_rows.clone();
        self.native_pending = true;
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let response = cx.prompt_for_new_path(&directory, Some("query-result.csv"));
        cx.spawn(async move |this, cx| {
            let response = response.await;
            let valid = this
                .update(cx, |this, cx| {
                    this.native_pending = false;
                    let valid = !this.closed && permit.is_valid();
                    cx.notify();
                    valid
                })
                .unwrap_or(false);
            drop(permit);
            if !valid {
                return;
            }
            let path = match response {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(i18n_key::TERMINAL_COULD_NOT_OPEN_THE_SAVE_DIALOG_PLEASE_TRY_AGAIN.into());
                        cx.notify();
                    });
                    return;
                }
            };
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let parent = path
                        .parent()
                        .ok_or_else(|| io::Error::other("missing export directory"))?;
                    let mut file = tempfile::NamedTempFile::new_in(parent)?;
                    write_delimited(file.as_file_mut(), &output.results[index], &rows, b',')?;
                    file.as_file_mut().sync_all()?;
                    file.persist_noclobber(path).map_err(|error| error.error)?;
                    Ok::<_, io::Error>(())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.closed {
                    return;
                }
                match saved {
                    Ok(()) => {
                        this.show_success(i18n_key::DATABASE_CSV_EXPORTED_NULL_IS_WRITTEN_AS_N, cx);
                        this.error = None;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        this.error = Some(i18n_key::DATABASE_THE_EXPORT_FILE_ALREADY_EXISTS_CHOOSE_A_NEW_FILENAME.into());
                    }
                    Err(_) => {
                        this.error = Some(i18n_key::DATABASE_COULD_NOT_EXPORT_THE_RESULT_CHECK_THE_DESTINATION_PERMISSIONS_AND.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

pub(super) fn visible_rows(
    result: &ResultSet,
    needle: &str,
    sort: Option<(usize, bool)>,
) -> Vec<usize> {
    let mut indices: Vec<_> = result
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            needle.is_empty()
                || row.iter().any(|cell| {
                    cell.as_deref()
                        .unwrap_or("NULL")
                        .to_lowercase()
                        .contains(needle)
                })
        })
        .map(|(index, _)| index)
        .collect();
    if let Some((column, descending)) = sort {
        indices.sort_by(|left, right| {
            let order = compare_cells(
                result.rows[*left].get(column).and_then(Option::as_deref),
                result.rows[*right].get(column).and_then(Option::as_deref),
            );
            if descending { order.reverse() } else { order }
        });
    }
    indices
}

// Sort text exactly as returned by the server; do not round large integers or decimals to f64.
fn compare_cells(left: Option<&str>, right: Option<&str>) -> Ordering {
    left.cmp(&right)
}

fn write_delimited(
    writer: &mut impl Write,
    result: &ResultSet,
    rows: &[usize],
    separator: u8,
) -> io::Result<()> {
    for (index, column) in result.columns.iter().enumerate() {
        if index != 0 {
            writer.write_all(&[separator])?;
        }
        write_field(writer, Some(column), separator)?;
    }
    writer.write_all(b"\r\n")?;
    for &row in rows {
        for (index, cell) in result.rows[row].iter().enumerate() {
            if index != 0 {
                writer.write_all(&[separator])?;
            }
            write_field(writer, cell.as_deref(), separator)?;
        }
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

fn write_field(writer: &mut impl Write, value: Option<&str>, separator: u8) -> io::Result<()> {
    let Some(value) = value else {
        return writer.write_all(b"\\N");
    };
    let quote = value.is_empty()
        || value == "\\N"
        || value
            .bytes()
            .any(|byte| matches!(byte, b'"' | b'\r' | b'\n') || byte == separator);
    if quote {
        writer.write_all(b"\"")?;
    }
    for chunk in value.split_inclusive('"') {
        writer.write_all(chunk.as_bytes())?;
        if chunk.ends_with('"') {
            writer.write_all(b"\"")?;
        }
    }
    if quote {
        writer.write_all(b"\"")?;
    }
    Ok(())
}

struct LimitedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("clipboard limit"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delimited_export_preserves_null_empty_quotes_and_newlines() {
        let result = ResultSet {
            columns: vec![
                "a,b".into(),
                "empty".into(),
                "null".into(),
                "literal".into(),
            ],
            rows: vec![vec![
                Some("x\"y\nz".into()),
                Some(String::new()),
                None,
                Some("\\N".into()),
            ]],
            affected_rows: 0,
            truncated: false,
        };
        let mut output = Vec::new();
        write_delimited(&mut output, &result, &[0], b',').unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"a,b\",empty,null,literal\r\n\"x\"\"y\nz\",\"\",\\N,\"\\N\"\r\n"
        );
    }
    #[test]
    fn loaded_row_filter_and_sort_keep_server_values_exact() {
        let result = ResultSet {
            columns: vec!["value".into()],
            rows: vec![
                vec![Some("9007199254740993".into())],
                vec![None],
                vec![Some("9007199254740992".into())],
                vec![Some(String::new())],
            ],
            affected_rows: 0,
            truncated: false,
        };
        assert_eq!(
            visible_rows(&result, "900719925", Some((0, false))),
            vec![2, 0]
        );
        assert_eq!(visible_rows(&result, "null", None), vec![1]);
        assert_eq!(
            visible_rows(&result, "", Some((0, false))),
            vec![1, 3, 2, 0]
        );
    }
}
