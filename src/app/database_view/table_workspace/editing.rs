use super::*;
use rustterm::i18n::messages as i18n_key;

impl DatabaseView {
    pub(in crate::app::database_view) fn paste_cell(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_editable() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if !crate::app::text_input::can_edit_literal(&text) {
            self.documents[self.active_document].error = Some(
                i18n_key::DATABASE_THIS_VALUE_CANNOT_BE_EDITED_WITHOUT_CHANGING_ITS_CONTENTS.into(),
            );
            cx.notify();
            return;
        }
        if self
            .table_workspace()
            .and_then(|table| table.edit.as_ref())
            .is_none_or(|edit| !edit.expanded)
            && !crate::app::text_input::can_edit_inline_literal(&text)
        {
            self.documents[self.active_document].error =
                Some(i18n_key::DATABASE_USE_THE_EXPANDED_EDITOR_FOR_MULTILINE_TEXT.into());
            cx.notify();
            return;
        }
        if !self.table_workspace().is_some_and(TableWorkspace::editing) {
            self.edit_cell(window, cx);
        }
        if let Some(edit) = self.documents[self.active_document]
            .table
            .as_mut()
            .and_then(|table| table.edit.as_mut())
        {
            if edit.input.read(cx).is_composing() {
                return;
            }
            edit.mode = CellMode::Value;
            edit.input.update(cx, |input, cx| {
                input.set_value(text, cx);
                input.set_placeholder(i18n_key::DATABASE_CELL_VALUE, cx);
            });
            edit.input.read(cx).focus(window);
            self.documents[self.active_document].error = None;
            cx.notify();
        }
    }

    pub(in crate::app::database_view) fn can_edit_cell(&self, row: usize, column: usize) -> bool {
        if !self.table_editable() {
            return false;
        }
        let Some(value) = self
            .current_result()
            .and_then(|result| result.rows.get(row))
            .and_then(|row| row.get(column))
        else {
            return false;
        };
        if value
            .as_deref()
            .is_some_and(|value| !crate::app::text_input::can_edit_literal(value))
        {
            return false;
        }
        let table = self.table_workspace().unwrap();
        let schema = table.schema.as_ref().unwrap();
        schema
            .columns
            .get(column)
            .is_some_and(|column| column.editable())
            && !matches!(table.changes.get(&row), Some(RowChange::Delete { .. }))
            && (matches!(table.changes.get(&row), Some(RowChange::Insert { .. }))
                || table
                    .page
                    .as_ref()
                    .and_then(|page| page.originals.get(row))
                    .is_some_and(|row| schema.row_editable(row)))
    }

    pub(in crate::app::database_view) fn can_edit_inline_cell(
        &self,
        row: usize,
        column: usize,
    ) -> bool {
        self.can_edit_cell(row, column)
            && self
                .current_result()
                .and_then(|result| result.rows.get(row))
                .and_then(|row| row.get(column))
                .is_some_and(|value| {
                    value
                        .as_deref()
                        .is_none_or(crate::app::text_input::can_edit_inline_literal)
                })
            && self
                .table_workspace()
                .and_then(|table| table.schema.as_ref())
                .is_some_and(|schema| {
                    schema.columns[column].kind != rustterm::database::table::ValueKind::Binary
                })
    }

    pub(in crate::app::database_view) fn cell_color(
        &self,
        row: usize,
        column: usize,
        null: bool,
    ) -> u32 {
        if let Some(table) = self
            .table_workspace()
            .filter(|table| table.section == TableAction::Data)
        {
            match table.changes.get(&row) {
                Some(RowChange::Delete { .. }) => return ui::colors().danger,
                Some(RowChange::Insert { .. }) => return ui::colors().success,
                Some(RowChange::Update { values, .. })
                    if values.iter().any(|(index, _)| *index == column) =>
                {
                    return ui::colors().warning;
                }
                _ => {}
            }
        }
        if null {
            ui::colors().faint
        } else {
            ui::colors().text
        }
    }

    pub(in crate::app::database_view) fn cell_preview(
        &self,
        row: usize,
        column: usize,
        value: Option<&str>,
    ) -> String {
        if let Some(table) = self
            .table_workspace()
            .filter(|table| table.section == TableAction::Data)
            && let Some(RowChange::Insert { values }) = table.changes.get(&row)
            && !values.iter().any(|(index, _)| *index == column)
        {
            return "DEFAULT".into();
        }
        super::super::render::preview(value, 256)
    }

    pub(in crate::app::database_view) fn table_editable(&self) -> bool {
        self.available()
            && !matches!(&self.config, DatabaseConfig::Sqlite(config) if config.read_only)
            && self.table_workspace().is_some_and(|table| {
                table.section == TableAction::Data
                    && !table.submission_uncertain
                    && table.schema.as_ref().is_some_and(TableSchema::editable)
            })
    }

    pub(in crate::app::database_view) fn edit_cell(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_cell_edit(false, window, cx);
    }

    fn begin_cell_edit(&mut self, expanded: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table_editable() {
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        let document = &self.documents[self.active_document];
        let Some((row, column)) = document.selected_cell else {
            return;
        };
        if !self.can_edit_cell(row, column) {
            return;
        }
        let table = document.table.as_ref().unwrap();
        let Some(value) = document
            .current_result()
            .and_then(|result| result.rows.get(row))
            .and_then(|row| row.get(column))
        else {
            return;
        };
        let mode = changes::initial_mode(table.changes.get(&row), column, value.as_deref());
        let value = value.clone().unwrap_or_default();
        if !expanded && !self.can_edit_inline_cell(row, column) {
            self.documents[self.active_document].error = Some(
                i18n_key::DATABASE_USE_THE_EXPANDED_EDITOR_FOR_MULTILINE_OR_BINARY_VALUES.into(),
            );
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            let mut input = TextInput::new(mode.placeholder(), false, cx);
            if expanded {
                input.set_literal(true, cx);
            } else {
                input.set_inline_literal(cx);
            }
            if mode == CellMode::Value {
                input.set_value(value, cx);
            }
            input.select_all(cx);
            input
        });
        let edit = CellEdit {
            row,
            column,
            input,
            mode,
            expanded,
        };
        let subscription = edit.bind(document.id, cx);
        if !expanded || mode == CellMode::Value {
            edit.input.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        let table = self.documents[self.active_document].table.as_mut().unwrap();
        table.edit = Some(edit);
        table.edit_subscription = Some(subscription);
        self.documents[self.active_document].error = None;
        self.reveal_grid_cell(row, column, cx);
        cx.notify();
    }

    pub(in crate::app::database_view) fn inline_edit_cell(&self) -> Option<(usize, usize)> {
        let edit = self.table_workspace()?.edit.as_ref()?;
        (!edit.expanded).then_some((edit.row, edit.column))
    }

    pub(in crate::app::database_view) fn cell_edit_focused(
        &self,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.table_workspace()
            .and_then(|table| table.edit.as_ref())
            .is_some_and(|edit| {
                edit.input.read(cx).focus_handle().is_focused(window)
                    || (edit.expanded
                        && edit.mode != CellMode::Value
                        && self.focus.is_focused(window))
            })
    }

    pub(in crate::app::database_view) fn select_grid_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.interactive() {
            return false;
        }
        if let Some(edit) = self.table_workspace().and_then(|table| table.edit.as_ref()) {
            if (edit.row, edit.column) == (row, column) {
                if !self.table_workspace().unwrap().focus_edit(window, cx) {
                    self.focus.focus(window);
                }
                return true;
            }
            if !self.stage_cell(window, cx) {
                if let Some(table) = self.table_workspace() {
                    table.focus_edit(window, cx);
                }
                return false;
            }
        }
        self.documents[self.active_document].selected_cell = Some((row, column));
        self.focus.focus(window);
        cx.notify();
        true
    }

    pub(in crate::app::database_view) fn cancel_cell_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.input_composing(cx) {
            return;
        }
        if let Some(table) = self.documents[self.active_document].table.as_mut() {
            table.edit = None;
            table.edit_subscription = None;
            self.documents[self.active_document].error = None;
            self.documents[self.active_document].filter_rows(cx);
            self.focus.focus(window);
            cx.notify();
        }
    }

    pub(in crate::app::database_view) fn expand_cell_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.table_workspace().is_some_and(TableWorkspace::editing) {
            self.begin_cell_edit(true, window, cx);
        }
        let Some(edit) = self.documents[self.active_document]
            .table
            .as_mut()
            .and_then(|table| table.edit.as_mut())
        else {
            return;
        };
        if edit.input.read(cx).is_composing() {
            return;
        }
        edit.expanded = true;
        edit.input
            .update(cx, |input, cx| input.set_literal(true, cx));
        if edit.mode == CellMode::Value {
            edit.input.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    pub(super) fn set_cell_mode(
        &mut self,
        mode: CellMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.available() {
            return;
        }
        let Some(table) = self.documents[self.active_document].table.as_mut() else {
            return;
        };
        let Some(edit) = &mut table.edit else {
            return;
        };
        if edit.input.read(cx).is_composing() {
            return;
        }
        let column = &table.schema.as_ref().unwrap().columns[edit.column];
        if (mode == CellMode::Null && !column.nullable)
            || (mode == CellMode::Default
                && !matches!(table.changes.get(&edit.row), Some(RowChange::Insert { .. })))
        {
            return;
        }
        edit.mode = mode;
        edit.input.update(cx, |input, cx| {
            if mode != CellMode::Value {
                input.clear(cx);
            }
            input.set_placeholder(mode.placeholder(), cx);
            input.clear_validation_error(cx);
        });
        if !edit.expanded || mode == CellMode::Value {
            edit.input.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        self.documents[self.active_document].error = None;
        cx.notify();
    }

    pub(in crate::app::database_view) fn stage_cell(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.table_editable() {
            return false;
        }
        let document = &mut self.documents[self.active_document];
        let table = document.table.as_mut().unwrap();
        let Some(edit) = &table.edit else {
            return false;
        };
        if edit.input.read(cx).is_composing() {
            return false;
        }
        let schema = table.schema.as_ref().unwrap();
        let column = &schema.columns[edit.column];
        let value = if edit.mode == CellMode::Default {
            Ok(None)
        } else if edit.mode == CellMode::Null {
            if !column.nullable {
                document.error = Some(i18n_key::DATABASE_THIS_COLUMN_DOES_NOT_ALLOW_NULL.into());
                edit.input
                    .update(cx, |input, cx| input.set_invalid(true, cx));
                edit.input.read(cx).focus(window);
                cx.notify();
                return false;
            }
            Ok(Some(CellValue::Null))
        } else {
            let original = table
                .page
                .as_ref()
                .and_then(|page| page.originals.get(edit.row))
                .and_then(|row| row.get(edit.column));
            schema
                .parse_cell(edit.column, original, edit.input.read(cx).value())
                .map(Some)
        };
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                document.error = Some(Message::from_error(error));
                edit.input
                    .update(cx, |input, cx| input.set_invalid(true, cx));
                edit.input.read(cx).focus(window);
                cx.notify();
                return false;
            }
        };
        let row = edit.row;
        let column = edit.column;
        let originals = table
            .page
            .as_ref()
            .map_or(&[][..], |page| page.originals.as_slice());
        let display = match changes::stage_value(&mut table.changes, originals, row, column, value)
        {
            Ok(display) => display,
            Err(error) => {
                document.error = Some(Message::from_error(error));
                edit.input
                    .update(cx, |input, cx| input.set_invalid(true, cx));
                edit.input.read(cx).focus(window);
                cx.notify();
                return false;
            }
        };
        if let Some(output) = &mut document.output {
            Arc::make_mut(output).results[0].rows[row][column] = display;
        }
        table.edit = None;
        table.edit_subscription = None;
        document.error = None;
        document.result_revision += 1;
        document.filter_rows(cx);
        self.focus.focus(window);
        cx.notify();
        true
    }

    pub(super) fn add_table_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table_editable() {
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        let document = &mut self.documents[self.active_document];
        let table = document.table.as_mut().unwrap();
        let Some(schema) = &table.schema else { return };
        let column = schema.columns.iter().position(|column| column.editable());
        let Some(column) = column else { return };
        let Some(output) = &mut document.output else {
            return;
        };
        let result = &mut Arc::make_mut(output).results[0];
        let row = result.rows.len();
        result.rows.push(vec![None; schema.columns.len()]);
        table
            .changes
            .insert(row, RowChange::Insert { values: Vec::new() });
        document.visible_rows.push(row);
        document.selected_cell = Some((row, column));
        document.result_scroll.scroll_to_item(
            document.visible_rows.len() - 1,
            gpui::ScrollStrategy::Bottom,
        );
        document.result_revision += 1;
        self.edit_cell(window, cx);
    }

    pub(in crate::app::database_view) fn delete_table_row(&mut self, cx: &mut Context<Self>) {
        if !self.table_editable() {
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        let document = &mut self.documents[self.active_document];
        let Some((row, _)) = document.selected_cell else {
            return;
        };
        let table = document.table.as_mut().unwrap();
        let originals = table
            .page
            .as_ref()
            .map_or(&[][..], |page| page.originals.as_slice());
        if !matches!(table.changes.get(&row), Some(RowChange::Insert { .. }))
            && !originals
                .get(row)
                .is_some_and(|original| table.schema.as_ref().unwrap().row_editable(original))
        {
            document.error = Some(i18n_key::DATABASE_THIS_ROW_HAS_NO_RELIABLE_UNIQUE_KEY.into());
            cx.notify();
            return;
        }
        match changes::toggle_delete(&mut table.changes, originals, row) {
            Ok(changes::DeleteEffect::RemoveInserted) => {
                if let Some(output) = &mut document.output {
                    Arc::make_mut(output).results[0].rows.remove(row);
                }
                document.selected_cell = None;
                document.filter_rows(cx);
            }
            Ok(changes::DeleteEffect::ShowOriginal) => {
                if let Some(output) = &mut document.output {
                    Arc::make_mut(output).results[0].rows[row] =
                        originals[row].iter().map(CellValue::display).collect();
                }
            }
            Err(error) => document.error = Some(Message::from_error(error)),
        }
        document.result_revision += 1;
        cx.notify();
    }

    pub(super) fn discard_table_changes(&mut self, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        let document = &mut self.documents[self.active_document];
        let Some(table) = &mut document.table else {
            return;
        };
        table.changes.clear();
        table.edit = None;
        table.edit_subscription = None;
        table.review = false;
        table.preview.clear();
        if table.submission_uncertain {
            table.submission_uncertain = false;
            let request = table.request.clone();
            table.page = None;
            table.schema = None;
            document.output = None;
            self.load_table_page(request, cx);
            return;
        }
        if let Some(page) = &table.page {
            let result = page.result.clone();
            let elapsed = document
                .output
                .as_ref()
                .map_or(std::time::Duration::ZERO, |output| output.elapsed);
            document.install_output(
                QueryOutput {
                    results: vec![result],
                    elapsed,
                },
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn review_table_changes(&mut self, cx: &mut Context<Self>) {
        if !self.table_editable() {
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        let Some(client) = &self.client else { return };
        let document = &mut self.documents[self.active_document];
        let table = document.table.as_mut().unwrap();
        let Some(schema) = &table.schema else { return };
        let changes: Vec<_> = table.changes.values().cloned().collect();
        if changes.is_empty() {
            return;
        }
        match client.preview_changes(schema, &changes) {
            Ok(preview) => {
                table.preview = preview;
                table.review = true;
                table.edit = None;
                table.edit_subscription = None;
                document.error = None;
            }
            Err(error) => document.error = Some(Message::from_error(error)),
        }
        cx.notify();
    }

    pub(super) fn submit_table_changes(&mut self, cx: &mut Context<Self>) {
        if !self.table_editable() {
            return;
        }
        let document = &self.documents[self.active_document];
        let table = document.table.as_ref().unwrap();
        if !table.review || table.changes.is_empty() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(schema) = table.schema.clone() else {
            return;
        };
        let changes = table.changes.values().cloned().collect();
        let id = document.id;
        let generation = self.generation;
        let request = table.request.clone();
        self.busy = Some(i18n_key::DATABASE_SUBMITTING_CHANGES);
        self.running_document = Some(id);
        self.documents[self.active_document].error = None;
        let task = cx.background_executor().spawn(async move {
            client.apply_changes(&schema, changes).await?;
            let start = std::time::Instant::now();
            let refreshed = client.table_page(&schema, request).await;
            Ok::<_, DatabaseError>((refreshed, start.elapsed()))
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation { return; }
                this.busy = None; this.running_document = None;
                let Some(index) = this.document_index(id) else { return };
                let document = &mut this.documents[index];
                match result {
                    Ok((refreshed, elapsed)) => {
                        let table = document.table.as_mut().unwrap();
                        // Submission and refresh are separate outcomes. A failed refresh must never
                        // leave already-applied writes queued for resubmission.
                        table.changes.clear(); table.review = false; table.preview.clear();
                        match refreshed {
                            Ok(page) => {
                                let result = page.result.clone(); table.page = Some(page);
                                document.install_output(QueryOutput { results: vec![result], elapsed }, cx);
                            }
                            Err(_) => {
                                table.page = None;
                                document.output = None;
                                document.error = Some(i18n_key::DATABASE_CHANGES_WERE_SUBMITTED_BUT_REFRESHING_FAILED_RELOAD_THE_TABLE.into());
                            }
                        }
                    }
                    Err(error) => {
                        if matches!(error, DatabaseError::OutcomeUnknown | DatabaseError::Disconnected | DatabaseError::Protocol | DatabaseError::Runtime | DatabaseError::TransactionRecovery) {
                            document.table.as_mut().unwrap().submission_uncertain = true;
                        }
                        document.error = Some(Message::from_error(error));
                    }
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl CellMode {
    fn placeholder(self) -> &'static str {
        match self {
            Self::Value => i18n_key::DATABASE_CELL_VALUE,
            Self::Null => "NULL",
            Self::Default => "DEFAULT",
        }
    }
}

impl CellEdit {
    pub(super) fn bind(&self, id: u64, cx: &mut Context<DatabaseView>) -> Subscription {
        cx.subscribe(&self.input, move |this, input, event, cx| {
            let Some(index) = this.document_index(id) else {
                return;
            };
            let document = &mut this.documents[index];
            let Some(edit) = document
                .table
                .as_mut()
                .and_then(|table| table.edit.as_mut())
            else {
                return;
            };
            if edit.input != input {
                return;
            }
            match event {
                InputEvent::Changed => {
                    edit.mode = CellMode::Value;
                    edit.input.update(cx, |input, cx| {
                        input.set_placeholder(i18n_key::DATABASE_CELL_VALUE, cx);
                        input.set_invalid(false, cx);
                    });
                    document.error = None;
                }
                InputEvent::Rejected(message) => document.error = Some((*message).into()),
                InputEvent::Submit => return,
            }
            cx.notify();
        })
    }
}
