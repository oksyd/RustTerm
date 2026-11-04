use super::*;

pub(in crate::app::database_view) struct TableSnapshot {
    info: TableInfo,
    schema: Option<TableSchema>,
    page: Option<TablePage>,
    request: PageRequest,
    changes: BTreeMap<usize, RowChange>,
    submission_uncertain: bool,
    section: TableAction,
    sections: HashMap<TableAction, GridSnapshot>,
    filter_input: Entity<TextInput>,
    filter_column: usize,
    filter_operator: FilterOperator,
    edit: Option<CellEdit>,
}

#[derive(PartialEq, Eq)]
pub(in crate::app::database_view) struct DiscardState {
    changes: BTreeMap<usize, RowChange>,
    submission_uncertain: bool,
    edit: Option<(usize, usize, String, CellMode)>,
}

impl TableWorkspace {
    pub(in crate::app::database_view) fn snapshot(&self) -> TableSnapshot {
        TableSnapshot {
            info: self.info.clone(),
            schema: self.schema.clone(),
            page: self.page.clone(),
            request: self.request.clone(),
            changes: self.changes.clone(),
            submission_uncertain: self.submission_uncertain,
            section: self.section,
            sections: self.sections.clone(),
            filter_input: self.filter_input.clone(),
            filter_column: self.filter_column,
            filter_operator: self.filter_operator,
            edit: self.edit.clone(),
        }
    }

    pub(in crate::app::database_view) fn discard_state(&self, cx: &App) -> DiscardState {
        DiscardState {
            changes: self.changes.clone(),
            submission_uncertain: self.submission_uncertain,
            edit: self.edit.as_ref().map(|edit| {
                (
                    edit.row,
                    edit.column,
                    edit.input.read(cx).value().to_owned(),
                    edit.mode,
                )
            }),
        }
    }

    pub(in crate::app::database_view) fn has_local_edit(&self) -> bool {
        !self.changes.is_empty() || self.edit.is_some()
    }

    pub(in crate::app::database_view) fn focus_edit(&self, window: &mut Window, cx: &App) -> bool {
        if let Some(edit) = &self.edit
            && (!edit.expanded || edit.mode == CellMode::Value)
        {
            edit.input.read(cx).focus(window);
            true
        } else {
            false
        }
    }
}

impl TableSnapshot {
    pub(in crate::app::database_view) fn restore(
        self,
        id: u64,
        cx: &mut Context<DatabaseView>,
    ) -> TableWorkspace {
        let subscription = cx.subscribe(&self.filter_input, move |this, _, event, cx| {
            if matches!(event, InputEvent::Submit)
                && this
                    .documents
                    .get(this.active_document)
                    .is_some_and(|document| document.id == id)
            {
                this.apply_table_filter(cx);
            }
        });
        let edit_subscription = self.edit.as_ref().map(|edit| edit.bind(id, cx));
        TableWorkspace {
            info: self.info,
            schema: self.schema,
            page: self.page,
            request: self.request,
            changes: self.changes,
            submission_uncertain: self.submission_uncertain,
            section: self.section,
            sections: self.sections,
            filter_input: self.filter_input,
            filter_column: self.filter_column,
            filter_operator: self.filter_operator,
            picker: None,
            edit: self.edit,
            edit_subscription,
            review: false,
            preview: Vec::new(),
            _subscription: subscription,
        }
    }
}
