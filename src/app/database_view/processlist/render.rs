use super::*;
use gpui::{AnyElement, MouseButton, div, list, prelude::*, px, rgb, uniform_list};
use rustterm::database::processlist::Visibility;
use ui::{ButtonStyle, Icon};

const WIDTHS: [f32; 8] = [68., 96., 136., 112., 88., 84., 136., 360.];
const LABELS: [&str; 8] = [
    "ID",
    i18n_key::DATABASE_SESSIONS_USER,
    i18n_key::DATABASE_SESSIONS_HOST,
    i18n_key::DATABASE_DATABASE_NAME,
    i18n_key::DATABASE_SESSIONS_COMMAND,
    i18n_key::DATABASE_SESSIONS_TIME,
    i18n_key::DATABASE_SESSIONS_STATE,
    "SQL",
];

impl DatabaseView {
    pub(in crate::app::database_view) fn render_sessions_tab(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        self.monitor.as_ref()?;
        Some(
            div()
                .id("database-sessions-tab")
                .h_full()
                .px_3()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .tab_index(0)
                .border_b_2()
                .border_color(rgb(if self.page == WorkspacePage::Sessions {
                    self.engine_color()
                } else {
                    ui::colors().chrome
                }))
                .when(self.page == WorkspacePage::Sessions, |d| {
                    d.bg(rgb(ui::colors().bg))
                })
                .hover(|s| s.bg(rgb(ui::colors().hover)))
                .child(ui::icon(Icon::Pulse).text_color(rgb(self.engine_color())))
                .child(tr(i18n_key::DATABASE_SESSIONS))
                .on_click(cx.listener(|this, _, window, cx| this.open_sessions(window, cx)))
                .into_any_element(),
        )
    }
    pub(in crate::app::database_view) fn render_sessions(&self, cx: &Context<Self>) -> AnyElement {
        let monitor = self.monitor.as_ref().unwrap();
        let enabled = self.monitor_connected();
        let mut body = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(100.))
                            .text_size(px(14.))
                            .child(tr(i18n_key::DATABASE_SESSIONS_TITLE)),
                    )
                    .child(
                        div()
                            .text_color(rgb(ui::colors().muted))
                            .text_size(px(11.))
                            .child(tr(i18n_key::DATABASE_SESSIONS_REFRESH)),
                    )
                    .children([0, 2, 5, 10].into_iter().map(|seconds| {
                        let label = if seconds == 0 {
                            tr(i18n_key::DATABASE_SESSIONS_MANUAL).into()
                        } else {
                            rustterm::trf!(i18n_key::DATABASE_SESSIONS_INTERVAL, seconds = seconds)
                        };
                        div()
                            .id(("session-refresh-interval", seconds as usize))
                            .px_2()
                            .py_1()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .tab_index(0)
                            .hover(|s| s.bg(rgb(ui::colors().hover)))
                            .child(label)
                            .when(monitor.refresh_secs == seconds, |b| {
                                b.bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().accent))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.interactive() {
                                    return;
                                }
                                this.monitor.as_mut().unwrap().refresh_secs = seconds;
                                this.start_monitor_poll(cx);
                                if seconds > 0 {
                                    this.refresh_sessions(cx);
                                }
                                cx.notify();
                            }))
                    }))
                    .child(
                        ui::icon_button_enabled(
                            "refresh-sessions",
                            Icon::Refresh,
                            i18n_key::DATABASE_SESSIONS_REFRESH_NOW,
                            enabled && !monitor.busy,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_sessions(cx))),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px((self.viewport_size.0 - 278.).max(180.)))
                            .min_w(px(180.))
                            .child(monitor.search.clone()),
                    )
                    .child(div().w(px(160.)).child(monitor.database.clone()))
                    .child(
                        ui::text_button(
                            "sessions-hide-sleep",
                            i18n_key::DATABASE_SESSIONS_HIDE_SLEEP,
                            ButtonStyle::Neutral,
                        )
                        .when(monitor.hide_sleep, |b| {
                            b.bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().accent))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.interactive() {
                                return;
                            }
                            let monitor = this.monitor.as_mut().unwrap();
                            monitor.hide_sleep = !monitor.hide_sleep;
                            monitor.filter(cx);
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(match monitor.snapshot.as_ref().map(|s| s.visibility) {
                        Some(Visibility::All) => i18n_key::DATABASE_SESSIONS_ALL_VISIBLE,
                        Some(Visibility::Own) => i18n_key::DATABASE_SESSIONS_OWN_VISIBLE,
                        _ => i18n_key::DATABASE_SESSIONS_SCOPE_UNKNOWN,
                    })),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(div().flex_1().child(rustterm::trf!(
                        i18n_key::DATABASE_SESSIONS_COUNTS,
                        shown = monitor.rows.len(),
                        total = monitor.snapshot.as_ref().map_or(0, |s| s.processes.len())
                    )))
                    .when(monitor.busy, |d| {
                        d.child(tr(i18n_key::DATABASE_SESSIONS_REFRESHING))
                    })
                    .when_some(monitor.updated.as_ref(), |d, at| {
                        d.child(rustterm::trf!(
                            i18n_key::DATABASE_SESSIONS_UPDATED,
                            time = at.as_str()
                        ))
                    })
                    .when(
                        monitor.snapshot.as_ref().is_some_and(|s| s.truncated),
                        |d| d.child(tr(i18n_key::DATABASE_RESULT_TRUNCATED)),
                    ),
            );
        if let Some(error) = &monitor.error {
            body = body.child(
                div()
                    .id("sessions-error")
                    .px_3()
                    .py_2()
                    .max_h(px(64.))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .text_color(rgb(ui::colors().danger))
                    .child(error.render())
                    .child(format!(
                        " {}",
                        tr(i18n_key::DATABASE_SESSIONS_REFRESH_STOPPED)
                    ))
                    .map(|d| ui::scrollable(d, None)),
            );
        }
        if monitor.rows.is_empty() {
            body = body.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(if monitor.busy {
                        i18n_key::DATABASE_SESSIONS_REFRESHING
                    } else if !enabled {
                        i18n_key::DATABASE_SESSIONS_DISCONNECTED
                    } else {
                        i18n_key::DATABASE_SESSIONS_EMPTY
                    })),
            );
        } else {
            body = body.child(self.render_session_grid(cx));
        }
        if monitor.selected.is_some() {
            body = body.child(self.render_session_sql(cx));
        }
        body.into_any_element()
    }
    fn render_session_grid(&self, cx: &Context<Self>) -> AnyElement {
        let monitor = self.monitor.as_ref().unwrap();
        let width: f32 = WIDTHS.iter().sum();
        let table = div()
            .w(px(width))
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(30.))
                    .flex_shrink_0()
                    .flex()
                    .bg(rgb(ui::colors().raised))
                    .children(LABELS.iter().enumerate().map(|(column, &label)| {
                        let suffix = if monitor.sort.0 == column {
                            if monitor.sort.1 { " ↓" } else { " ↑" }
                        } else {
                            ""
                        };
                        div()
                            .id(("session-column", column))
                            .w(px(WIDTHS[column]))
                            .h_full()
                            .flex_shrink_0()
                            .px_2()
                            .flex()
                            .items_center()
                            .border_r_1()
                            .border_color(rgb(ui::colors().border))
                            .cursor_pointer()
                            .tab_index(0)
                            .hover(|s| s.bg(rgb(ui::colors().hover)))
                            .child(div().truncate().child(format!("{}{suffix}", tr(label))))
                            .when(column == 5, |d| {
                                d.tooltip(|_, cx| {
                                    cx.new(|_| {
                                        ui::Tooltip(
                                            tr(i18n_key::DATABASE_SESSIONS_TIME_HINT).into(),
                                        )
                                    })
                                    .into()
                                })
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.interactive() {
                                    return;
                                }
                                let monitor = this.monitor.as_mut().unwrap();
                                monitor.sort = (
                                    column,
                                    if monitor.sort.0 == column {
                                        !monitor.sort.1
                                    } else {
                                        column == 5
                                    },
                                );
                                monitor.filter(cx);
                                cx.notify();
                            }))
                    })),
            )
            .child(
                uniform_list(
                    "mysql-session-rows",
                    monitor.rows.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| this.render_session_row(index, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .map(ui::preserve_scroll_axes)
                .track_scroll(monitor.scroll.clone())
                .w(px(width))
                .flex_1()
                .min_h_0(),
            );
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .pr(px(ui::SCROLLBAR_WIDTH))
            .pb(px(ui::SCROLLBAR_WIDTH))
            .child(
                div()
                    .id("session-horizontal")
                    .track_scroll(&monitor.horizontal)
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_x_scroll()
                    .map(ui::preserve_scroll_axes)
                    .child(table),
            )
            .child(ui::scrollbars(
                "session-grid-bars",
                Some(monitor.horizontal.clone()),
                Some(monitor.scroll.0.borrow().base_handle.clone()),
            ))
            .into_any_element()
    }
    fn render_session_row(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let monitor = self.monitor.as_ref().unwrap();
        let process = &monitor.snapshot.as_ref().unwrap().processes[monitor.rows[index]];
        let id = process.id;
        let time = process.seconds.map(|s| s.to_string());
        let id_text = id.to_string();
        let cells = [
            Some(id_text.as_str()),
            Some(process.user.as_str()),
            Some(process.host.as_str()),
            process.database.as_deref(),
            Some(process.command.as_str()),
            time.as_deref(),
            process.state.as_deref(),
            process.sql.as_deref(),
        ];
        div()
            .id(("session-row", index))
            .h(px(30.))
            .flex()
            .cursor_pointer()
            .bg(rgb(if monitor.selected == Some(id) {
                ui::colors().selected
            } else if index.is_multiple_of(2) {
                ui::colors().surface
            } else {
                ui::colors().bg
            }))
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .children(cells.into_iter().enumerate().map(|(column, value)| {
                div()
                    .w(px(WIDTHS[column]))
                    .h_full()
                    .px_2()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
                    .text_color(rgb(if value.is_none() {
                        ui::colors().faint
                    } else {
                        ui::colors().text
                    }))
                    .child(
                        div()
                            .truncate()
                            .child(super::super::render::preview(value, 160)),
                    )
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if !this.interactive() {
                        return;
                    }
                    this.monitor.as_mut().unwrap().select(id);
                    this.focus.focus(window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .into_any_element()
    }
    fn render_session_sql(&self, cx: &Context<Self>) -> AnyElement {
        let monitor = self.monitor.as_ref().unwrap();
        let mut panel = div()
            .h(px((self.viewport_size.1 * 0.3).clamp(100., 220.)))
            .min_h_0()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .child(
                div()
                    .h(px(34.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(format!("SQL · #{}", monitor.selected.unwrap())),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "copy-session-sql",
                            Icon::Copy,
                            i18n_key::DATABASE_SESSIONS_COPY_SQL,
                            monitor.preview.is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.interactive()
                                && let Some(preview) =
                                    this.monitor.as_ref().and_then(|m| m.preview.as_ref())
                            {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    preview.sql.clone(),
                                ));
                            }
                        })),
                    )
                    .child(
                        ui::icon_button(
                            "close-session-sql",
                            Icon::Close,
                            i18n_key::DATABASE_SESSIONS_CLOSE_SQL,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let monitor = this.monitor.as_mut().unwrap();
                            monitor.selected = None;
                            monitor.preview = None;
                            cx.notify();
                        })),
                    ),
            );
        if let Some(preview) = &monitor.preview {
            panel = panel.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        list(
                            preview.scroll.clone(),
                            cx.processor(|this, index: usize, _, _| {
                                let preview =
                                    this.monitor.as_ref().unwrap().preview.as_ref().unwrap();
                                div()
                                    .px_3()
                                    .min_h(px(20.))
                                    .font_family(ui::MONOSPACE_FONT)
                                    .text_size(px(12.))
                                    .line_height(px(20.))
                                    .whitespace_normal()
                                    .child(preview.sql[preview.chunks[index].clone()].to_owned())
                                    .into_any_element()
                            }),
                        )
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .py_2(),
                    )
                    .child(ui::list_scrollbar(
                        "session-sql-scroll",
                        preview.scroll.clone(),
                    )),
            );
        } else {
            panel = panel.child(
                div()
                    .p_3()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::DATABASE_SESSIONS_NO_SQL)),
            );
        }
        panel.into_any_element()
    }
}
