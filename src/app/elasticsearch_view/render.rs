use super::*;
use crate::app::ui::{ButtonStyle, Icon};
use gpui::{AnyElement, IntoElement, MouseButton, Render, div, prelude::*, px, rgb, uniform_list};

impl Render for ElasticsearchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = self.viewport_size.0 < 760.;
        let header = ui::panel_header()
            .min_w_0()
            .child(ui::connection_address(
                "es-address",
                Icon::Elasticsearch,
                "Elasticsearch",
                self.config.endpoint.clone(),
                self.title.clone(),
            ))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(self.status_color()))
                    .child(self.status_text()),
            )
            .child(
                ui::icon_button("es-browser", Icon::Sidebar, msg::ES_INDICES).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.browser_open = !this.browser_open;
                        cx.notify();
                    }),
                ),
            )
            .child(
                ui::icon_button(
                    "es-connection",
                    if self.is_running() {
                        Icon::Disconnect
                    } else {
                        Icon::Refresh
                    },
                    if self.is_running() {
                        msg::COMMON_DISCONNECT
                    } else {
                        msg::COMMON_RECONNECT
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.disconnect_or_reconnect(cx))),
            )
            .when(self.pane_close_visible, |bar| {
                bar.child(
                    ui::icon_button("es-close", Icon::Close, msg::COMMON_CLOSE).on_click(
                        cx.listener(|this, _, _, cx| cx.emit(PaneEvent::Close(this.index))),
                    ),
                )
            });
        let mut body = div().flex().flex_1().min_h_0().min_w_0();
        if self.browser_open {
            body = body.child(self.render_browser(compact, cx));
        }
        if !compact || !self.browser_open {
            body = body.child(self.render_query(cx));
        }
        div()
            .id(("es-pane", self.index))
            .size_full()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .bg(rgb(ui::colors().bg))
            .text_color(rgb(ui::colors().text))
            .text_size(px(13.))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window);
                    cx.emit(PaneEvent::Focused(this.index));
                }),
            )
            .child(header)
            .when_some(self.error.clone(), |panel, error| {
                panel.child(
                    ui::feedback(error.render(), true)
                        .id("es-error")
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .map(ui::preserve_scroll_axes)
                        .map(|content| ui::scrollable(content, None)),
                )
            })
            .child(body)
    }
}

impl ElasticsearchView {
    fn render_browser(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let mut browser = div()
            .flex()
            .flex_col()
            .min_h_0()
            .flex_shrink_0()
            .border_r_1()
            .border_color(rgb(ui::colors().border))
            .when(compact, |panel| panel.flex_1().min_w_0())
            .when(!compact, |panel| panel.w(px(230.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .child(
                        ui::text_button(
                            "es-index-tab",
                            msg::ES_INDICES,
                            if !self.history_open {
                                ButtonStyle::Primary
                            } else {
                                ButtonStyle::Neutral
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_open = false;
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::text_button(
                            "es-history-tab",
                            msg::ES_HISTORY,
                            if self.history_open {
                                ButtonStyle::Primary
                            } else {
                                ButtonStyle::Neutral
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_open = true;
                            cx.notify();
                        })),
                    ),
            );
        if self.history_open {
            browser = browser.child(
                ui::text_button(
                    "es-clear-history",
                    msg::ES_CLEAR_HISTORY,
                    ButtonStyle::Neutral,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.window_state.is_locked() {
                        this.history.clear();
                        cx.notify();
                    }
                })),
            );
            if self.history.is_empty() {
                browser = browser.child(
                    div()
                        .p_3()
                        .text_color(rgb(ui::colors().muted))
                        .child(tr(msg::ES_HISTORY_EMPTY)),
                );
            }
            browser = browser.child(
                uniform_list(
                    "es-history-list",
                    self.history.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let query = &this.history[index];
                                div()
                                    .id(("es-history", index))
                                    .h(px(56.))
                                    .px_3()
                                    .py_1()
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                                    .child(
                                        div().truncate().child(
                                            query
                                                .index
                                                .clone()
                                                .unwrap_or_else(|| tr(msg::ES_ALL_INDICES).into()),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .truncate()
                                            .text_color(rgb(ui::colors().muted))
                                            .child(query.body.replace(['\n', '\r'], " ")),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if this.task.is_some() || this.window_state.is_locked() {
                                            return;
                                        }
                                        if let Some(query) = this.history.get(index).cloned() {
                                            this.select_index(query.index, cx);
                                            this.query.update(cx, |input, cx| {
                                                input.set_value(query.body, cx)
                                            });
                                            if this.viewport_size.0 < 760. {
                                                this.browser_open = false;
                                            }
                                            this.focus_query(window, cx);
                                            cx.notify();
                                        }
                                    }))
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .min_h_0(),
            );
        } else {
            browser = browser
                .child(div().p_2().child(self.filter.clone()))
                .child(
                    ui::text_button_enabled(
                        "es-refresh",
                        msg::ES_REFRESH,
                        ButtonStyle::Neutral,
                        self.available(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .child(
                    div()
                        .id("es-all-indices")
                        .px_3()
                        .py_2()
                        .cursor_pointer()
                        .when(self.selected.is_none(), |row| {
                            row.bg(rgb(ui::colors().selected))
                        })
                        .child(tr(msg::ES_ALL_INDICES))
                        .on_click(cx.listener(|this, _, _, cx| this.select_index(None, cx))),
                );
            let filter = self.filter.read(cx).value().to_lowercase();
            let indices: Arc<Vec<usize>> = Arc::new(
                self.indices
                    .iter()
                    .enumerate()
                    .filter_map(|(i, index)| {
                        index.index.to_lowercase().contains(&filter).then_some(i)
                    })
                    .collect(),
            );
            if indices.is_empty() {
                browser = browser.child(div().p_3().text_color(rgb(ui::colors().muted)).child(tr(
                    if self.task.is_some() {
                        msg::ES_LOADING
                    } else {
                        msg::ES_NO_INDICES
                    },
                )));
            }
            browser = browser.child(
                uniform_list(
                    "es-index-list",
                    indices.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|row| {
                                let index = &this.indices[indices[row]];
                                let name = index.index.clone();
                                let meta = index.attributes.join(" · ");
                                div()
                                    .id(("es-index", row))
                                    .h(px(48.))
                                    .px_3()
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .cursor_pointer()
                                    .when(this.selected.as_deref() == Some(&name), |row| {
                                        row.bg(rgb(ui::colors().selected))
                                    })
                                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                                    .child(div().truncate().child(name.clone()))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(11.))
                                            .text_color(rgb(ui::colors().muted))
                                            .child(meta),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_index(Some(name.clone()), cx)
                                    }))
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .min_h_0(),
            );
            if self.indices_truncated {
                browser = browser.child(
                    div()
                        .p_2()
                        .text_size(px(11.))
                        .child(tr(msg::ES_INDEX_LIMIT)),
                );
            }
        }
        browser.into_any_element()
    }

    fn select_index(&mut self, index: Option<String>, cx: &mut Context<Self>) {
        if self.task.is_some() || self.window_state.is_locked() {
            return;
        }
        if self.selected != index {
            self.selected = index;
            self.json = Arc::new(String::new());
            self.lines.clear();
            self.response_status = None;
        }
        if self.viewport_size.0 < 760. {
            self.browser_open = false;
        }
        cx.notify();
    }

    fn render_query(&self, cx: &mut Context<Self>) -> AnyElement {
        let heading = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .min_w_0()
            .child(
                div().flex_1().min_w_0().truncate().child(
                    self.selected
                        .clone()
                        .unwrap_or_else(|| tr(msg::ES_ALL_INDICES).into()),
                ),
            )
            .child(
                ui::text_button_enabled(
                    "es-mapping",
                    msg::ES_MAPPING,
                    ButtonStyle::Neutral,
                    self.available(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.run(true, cx))),
            )
            .child(
                ui::text_button_enabled(
                    "es-run",
                    msg::ES_RUN,
                    ButtonStyle::Primary,
                    self.available(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.run(false, cx))),
            )
            .when(self.task.is_some(), |bar| {
                bar.child(
                    ui::icon_button("es-cancel", Icon::Stop, msg::COMMON_CANCEL)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
            });
        let result_heading = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .child(
                div().flex_1().text_color(rgb(ui::colors().muted)).child(
                    self.response_status
                        .map(|(status, milliseconds)| {
                            rustterm::trf!(
                                msg::ES_RESPONSE_STATUS,
                                status = status,
                                milliseconds = milliseconds.to_string()
                            )
                        })
                        .unwrap_or_else(|| tr(msg::ES_RESULT).into()),
                ),
            )
            .child(
                ui::icon_button_enabled("es-copy", Icon::Copy, msg::ES_COPY, !self.json.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.copy_json(cx))),
            );
        let result = uniform_list(
            "es-json",
            self.lines.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|row| {
                        div()
                            .h(px(20.))
                            .flex()
                            .font_family(ui::MONOSPACE_FONT)
                            .text_size(px(12.))
                            .px_3()
                            .child(
                                div()
                                    .w(px(55.))
                                    .flex_shrink_0()
                                    .text_color(rgb(ui::colors().faint))
                                    .child((row + 1).to_string()),
                            )
                            .child(div().whitespace_nowrap().child(this.lines[row].clone()))
                    })
                    .collect()
            }),
        )
        .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
        .flex_1()
        .min_h_0();
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(heading)
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .h(px(196.))
                    .flex_shrink_0()
                    .child(self.query.clone()),
            )
            .child(result_heading)
            .when(self.lines.is_empty(), |panel| {
                panel.child(ui::empty_state(
                    Icon::Search,
                    tr(if self.task.is_some() {
                        msg::ES_LOADING
                    } else {
                        msg::ES_IDLE
                    }),
                ))
            })
            .child(result)
            .into_any_element()
    }
}
