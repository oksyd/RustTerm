use super::*;
use gpui::{AnyElement, CursorStyle, MouseButton, div, prelude::*, px, rgb};
use ui::{ButtonStyle, Icon};

impl DatabaseView {
    pub(super) fn editor_max_height(&self) -> f32 {
        (self.viewport_size.1 - 290.).max(100.)
    }
    pub(super) fn update_editor_height(&mut self, cx: &mut Context<Self>) {
        let height = self.editor_height.clamp(100., self.editor_max_height());
        for document in &self.documents {
            document
                .sql_input
                .update(cx, |input, cx| input.set_multiline_height(height, cx));
        }
    }
    pub(super) fn open_query_text(
        &mut self,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.interactive() {
            return;
        }
        self.new_query(window, cx);
        self.results_only = false;
        self.documents[self.active_document]
            .sql_input
            .update(cx, |input, cx| input.set_value(sql, cx));
        self.documents[self.active_document].editor_dirty = true;
        cx.notify();
    }
    pub(super) fn render_editor(&self, cx: &Context<Self>) -> AnyElement {
        match self.page {
            WorkspacePage::Sessions => return self.render_sessions(cx),
            WorkspacePage::Overview => return self.render_overview(cx),
            WorkspacePage::Document => {}
        }
        let document = &self.documents[self.active_document];
        if document.value_view.is_some() {
            return self.render_value(cx);
        }
        if document.table.is_some() {
            return self.render_table_workspace(cx);
        }
        let toolbar = div()
            .px_2().py_1().flex_shrink_0().flex().flex_wrap().items_center().gap_1()
            .child(
                render::action("database-run", i18n_key::DATABASE_RUN_STATEMENT, self.available(), ButtonStyle::Primary)
                    .tooltip(|_, cx| cx.new(|_| ui::Tooltip(tr(i18n_key::DATABASE_EXECUTE_SELECTION_OR_STATEMENT_AT_CURSOR_CTRL_ENTER).into())).into())
                    .on_click(cx.listener(|this, _, _, cx| this.run_sql(cx))),
            )
            .child(
                render::action("database-run-script", i18n_key::DATABASE_RUN_SCRIPT, self.available(), ButtonStyle::Neutral)
                    .on_click(cx.listener(|this, _, _, cx| this.run_script(cx))),
            )
            .when(self.busy.is_some(), |bar| bar.child(
                ui::icon_button("database-cancel", Icon::Stop, i18n_key::DATABASE_CANCEL_QUERY)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_query(cx))),
            ))
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(rgb(ui::colors().muted)).child(tr(i18n_key::DATABASE_ROW_LIMIT)))
            .child(div().w(px(64.)).child(document.row_limit.clone()))
            .child(
                ui::icon_button("database-expand-results",
                    if self.results_only { Icon::Restore } else { Icon::Maximize },
                    if self.results_only { i18n_key::DATABASE_SHOW_EDITOR } else { i18n_key::DATABASE_RESULTS_ONLY },
                ).on_click(cx.listener(|this, _, window, cx| {
                    if !this.interactive() { return; }
                    this.results_only = !this.results_only;
                    if this.results_only {
                        this.focus.focus(window);
                    } else {
                        this.documents[this.active_document].sql_input.read(cx).focus(window);
                    }
                    cx.notify();
                })),
            );
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(toolbar)
            .when(!self.results_only, |d| {
                d.child(document.sql_input.clone())
                    .child(
                        div()
                            .px_2()
                            .h(px(30.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .bg(rgb(ui::colors().surface))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(self.engine_color()))
                                    .child(self.engine().label()),
                            )
                            .child(
                                ui::icon_button_enabled(
                                    "query-history-previous",
                                    Icon::ChevronUp,
                                    i18n_key::DATABASE_PREVIOUS_QUERY,
                                    self.interactive() && !document.history.is_empty(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.recall(true, cx))),
                            )
                            .child(
                                ui::icon_button_enabled(
                                    "query-history-next",
                                    Icon::ChevronDown,
                                    i18n_key::DATABASE_NEXT_QUERY,
                                    self.interactive() && document.history_index.is_some(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.recall(false, cx))),
                            )
                            .child(div().flex_1())
                            .when(self.supports_transactions(), |bar| {
                                bar.child(self.render_transaction(cx))
                            }),
                    )
                    .child(
                        div()
                            .id("database-editor-divider")
                            .h(px(5.))
                            .w_full()
                            .flex_shrink_0()
                            .bg(rgb(ui::colors().border))
                            .cursor(CursorStyle::ResizeUpDown)
                            .hover(|s| s.bg(rgb(ui::colors().accent)))
                            .tooltip(|_, cx| {
                                cx.new(|_| ui::Tooltip(tr(i18n_key::DATABASE_EDITOR_RESIZE).into()))
                                    .into()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                    if this.interactive() {
                                        this.resize_drag = Some(ResizeDrag {
                                            target: sizing::ResizeTarget::Editor,
                                            start_x: f32::from(event.position.y),
                                            width: this
                                                .editor_height
                                                .clamp(100., this.editor_max_height()),
                                        });
                                    }
                                    window.prevent_default();
                                    cx.stop_propagation();
                                }),
                            ),
                    )
            })
            .child(self.render_results(cx))
            .into_any_element()
    }
}
