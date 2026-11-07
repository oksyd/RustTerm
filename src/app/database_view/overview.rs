use super::*;
use gpui::{AnyElement, div, prelude::*, px, rgb};
use ui::{ButtonStyle, Icon};

impl DatabaseView {
    pub(super) fn render_overview_tab(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_shrink_0()
            .id("database-overview-tab")
            .h_full()
            .px_3()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .tab_index(0)
            .border_b_2()
            .border_color(rgb(if self.page == WorkspacePage::Overview {
                self.engine_color()
            } else {
                ui::colors().surface
            }))
            .when(self.page == WorkspacePage::Overview, |d| {
                d.bg(rgb(ui::colors().bg))
            })
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .child(
                ui::icon(Icon::for_sql_engine(self.engine())).text_color(rgb(self.engine_color())),
            )
            .child(tr(i18n_key::DATABASE_OVERVIEW))
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.interactive() {
                    return;
                }
                this.pause_monitor();
                this.page = WorkspacePage::Overview;
                this.object_menu = None;
                this.focus.focus(window);
                cx.notify();
            }))
            .into_any_element()
    }
    pub(super) fn render_overview(&self, cx: &Context<Self>) -> AnyElement {
        let description = match self.engine() {
            SqlEngine::ClickHouse => i18n_key::DATABASE_CH_OVERVIEW,
            SqlEngine::Postgres => i18n_key::DATABASE_PG_OVERVIEW,
            SqlEngine::Mysql => i18n_key::DATABASE_MY_OVERVIEW,
            SqlEngine::Sqlite => i18n_key::DATABASE_SQLITE_OVERVIEW,
        };
        let (endpoint_label, endpoint) = match &self.config {
            DatabaseConfig::Sqlite(c) => {
                (i18n_key::DATABASE_LOCAL_FILE, c.path.display().to_string())
            }
            _ => (
                i18n_key::DATABASE_ENDPOINT,
                self.config
                    .server()
                    .map(|c| format!("{}:{}", c.host, c.port))
                    .unwrap_or_default(),
            ),
        };
        let transport = if self.engine() == SqlEngine::Sqlite {
            tr(i18n_key::DATABASE_LOCAL).to_owned()
        } else {
            match (
                self.ssh_route.is_some(),
                self.config.server().is_some_and(|c| c.tls),
            ) {
                (true, true) => "SSH · TLS".into(),
                (true, false) => "SSH".into(),
                (false, true) => "TLS".into(),
                _ => tr(i18n_key::DATABASE_DIRECT).into(),
            }
        };
        let views = self.tables.iter().filter(|t| dialect::is_view(t)).count();
        let schemas = self
            .tables
            .iter()
            .map(|t| &t.schema)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let content_width = self.viewport_size.0
            - if self.browser_open && self.viewport_size.0 >= BROWSER_MIN_WIDTH {
                self.browser_width() + 5.
            } else {
                0.
            };
        let value_width = (content_width - 180.).max(80.);
        let panel = div()
            .id("database-overview-content")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(44.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.))
                            .bg(rgb(ui::colors().raised))
                            .child(
                                ui::icon(Icon::for_sql_engine(self.engine()))
                                    .size(px(24.))
                                    .text_color(rgb(self.engine_color())),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_size(px(20.)).child(self.engine().label()))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(rgb(ui::colors().muted))
                                    .child(tr(description)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(metric(i18n_key::DATABASE_TABLES, self.tables.len() - views))
                    .child(metric(i18n_key::DATABASE_VIEWS, views))
                    .when(self.engine() == SqlEngine::Postgres, |d| {
                        d.child(metric(i18n_key::DATABASE_SCHEMAS, schemas))
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .bg(rgb(ui::colors().surface))
                    .overflow_hidden()
                    .child(detail(endpoint_label, endpoint, value_width))
                    .child(detail(
                        i18n_key::DATABASE_DATABASE_NAME,
                        self.database().into(),
                        value_width,
                    ))
                    .child(detail(i18n_key::DATABASE_TRANSPORT, transport, value_width))
                    .when_some(
                        match &self.config {
                            DatabaseConfig::Sqlite(c) => Some(c.read_only),
                            _ => None,
                        },
                        |d, read_only| {
                            d.child(detail(
                                i18n_key::DATABASE_ACCESS_MODE,
                                tr(if read_only {
                                    i18n_key::COMMON_READ_ONLY_BADGE
                                } else {
                                    i18n_key::DATABASE_READ_WRITE
                                })
                                .into(),
                                value_width,
                            ))
                        },
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        render::action(
                            "overview-new-query",
                            i18n_key::DATABASE_NEW_SQL_QUERY,
                            self.interactive(),
                            ButtonStyle::Primary,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.new_query(window, cx))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(rgb(ui::colors().faint))
                            .text_size(px(11.))
                            .child("SQL · Ctrl+Enter"),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(14.))
                            .child(tr(i18n_key::DATABASE_QUERY_TEMPLATES)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().muted))
                            .child(tr(i18n_key::DATABASE_TEMPLATES_HINT)),
                    ),
            )
            .children(
                dialect::templates(self.engine())
                    .into_iter()
                    .enumerate()
                    .map(|(i, template)| {
                        div()
                            .flex_shrink_0()
                            .id(("database-template", i))
                            .p_3()
                            .rounded(px(7.))
                            .border_1()
                            .border_color(rgb(ui::colors().border))
                            .bg(rgb(ui::colors().surface))
                            .cursor_pointer()
                            .tab_index(0)
                            .hover(|s| {
                                s.bg(rgb(ui::colors().hover))
                                    .border_color(rgb(self.engine_color()))
                            })
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                ui::icon(if i == 0 { Icon::Pulse } else { Icon::Database })
                                    .text_color(rgb(self.engine_color())),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(tr(template.label))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(11.))
                                            .text_color(rgb(ui::colors().muted))
                                            .child(tr(template.hint)),
                                    ),
                            )
                            .child(ui::icon(Icon::ChevronRight).text_color(rgb(ui::colors().muted)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_query_text(template.sql.into(), window, cx)
                            }))
                    }),
            );
        ui::scrollable(panel, None).into_any_element()
    }
}
fn metric(label: &'static str, count: usize) -> gpui::Div {
    div()
        .flex_shrink_0()
        .flex_1()
        .min_w(px(90.))
        .p_3()
        .rounded(px(7.))
        .bg(rgb(ui::colors().surface))
        .border_1()
        .border_color(rgb(ui::colors().border))
        .child(div().text_size(px(23.)).child(count.to_string()))
        .child(
            div()
                .flex_shrink_0()
                .mt_1()
                .text_size(px(11.))
                .text_color(rgb(ui::colors().muted))
                .child(tr(label)),
        )
}
fn detail(label: &'static str, value: String, width: f32) -> gpui::Stateful<gpui::Div> {
    let hint = value.clone();
    div()
        .id(label)
        .h(px(36.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(90.))
                .flex_shrink_0()
                .text_color(rgb(ui::colors().muted))
                .child(tr(label)),
        )
        .child(div().w(px(width)).truncate().child(value))
        .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(hint.clone())).into())
}
