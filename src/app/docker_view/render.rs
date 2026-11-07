use super::*;
use crate::app::ui::{self, ButtonStyle, Icon};
use gpui::{AnyElement, ClipboardItem, Div, MouseButton, Render, div, px, rgb, uniform_list};
impl Render for DockerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.interactive(cx);
        let mut body = div()
            .id("docker-workspace")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().raised))
            .text_color(rgb(ui::colors().text))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !window.default_prevented() {
                        this.focus.focus(window);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.interactive(cx) {
                    if this.selected.is_some() {
                        this.select(None, cx);
                    } else {
                        cx.emit(DockerEvent::Close);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                ui::panel_header()
                    .child(ui::icon(Icon::Container))
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            self.version
                                .as_ref()
                                .map(|v| format!("Docker {} · API {}", v.version, v.api))
                                .unwrap_or_else(|| tr(key::DOCKER_TITLE).into()),
                        ),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "docker-refresh",
                            Icon::Refresh,
                            key::DOCKER_REFRESH,
                            enabled && self.request.is_none() && self.action_task.is_none(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "docker-socket-toggle",
                            Icon::Settings,
                            key::DOCKER_SOCKET,
                            enabled && self.action_task.is_none(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_socket = !this.show_socket;
                            cx.notify();
                        })),
                    ),
            );
        if self.show_socket {
            body = body.child(
                div()
                    .px_3()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(tr(key::DOCKER_SOCKET))
                    .child(self.socket.clone())
                    .child(muted(tr(key::DOCKER_SOCKET_HINT)))
                    .child(
                        ui::text_button_enabled(
                            "docker-connect",
                            key::DOCKER_APPLY,
                            ButtonStyle::Primary,
                            enabled && self.action_task.is_none(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                    ),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                ui::feedback(error.clone(), true)
                    .id("docker-error")
                    .max_h(px(110.))
                    .overflow_y_scroll()
                    .map(|d| ui::scrollable(d, None)),
            );
        }
        if let Some(notice) = self.notice {
            body = body.child(
                div()
                    .px_3()
                    .py_1()
                    .text_color(rgb(ui::colors().success))
                    .child(tr(notice)),
            );
        }
        if self.selected.is_some() {
            body = body.child(self.container(cx));
        } else {
            body = body.child(self.container_list(cx));
        }
        body
    }
}
impl DockerView {
    fn container_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.containers.iter().filter(|c| c.running()).count();
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(div().px_3().pb_2().child(self.search.clone()))
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_color(rgb(ui::colors().muted))
                    .child(format!(
                        "{} / {} · {}",
                        running,
                        self.containers.len(),
                        tr(key::DOCKER_RUNNING)
                    )),
            );
        if self.rows.is_empty() {
            body = body.child(
                ui::empty_state(
                    Icon::Container,
                    tr(if self.request.is_some() {
                        key::DOCKER_LOADING
                    } else if self.failed {
                        key::DOCKER_UNAVAILABLE
                    } else {
                        key::DOCKER_EMPTY
                    }),
                )
                .child(muted(tr(key::DOCKER_CONNECTING_HELP))),
            );
        } else {
            body = body.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        gpui::list(
                            self.list_scroll.clone(),
                            cx.processor(|this, index: usize, _, cx| this.container_row(index, cx)),
                        )
                        .size_full(),
                    )
                    .child(ui::list_scrollbar(
                        "docker-list-scroll",
                        self.list_scroll.clone(),
                    )),
            );
        }
        body.into_any_element()
    }
    fn container_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match &self.rows[index] {
            ListRow::Project(project, count) => div()
                .w_full()
                .h(px(32.))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .border_b_1()
                .border_color(rgb(ui::colors().border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(ui::colors().muted))
                        .child(if project.is_empty() {
                            tr(key::DOCKER_UNASSIGNED).to_owned()
                        } else {
                            project.clone()
                        }),
                )
                .child(count.to_string())
                .into_any_element(),
            ListRow::Container(item) => {
                let container = &self.containers[*item];
                let id = container.id.clone();
                let key_id = id.clone();
                let detail = format!(
                    "{}\n{}\n{}",
                    container.image,
                    container.status,
                    container.ports()
                );
                div()
                    .id(("docker-container", index))
                    .w_full()
                    .h(px(68.))
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(ui::colors().hover)))
                    .tab_index(0)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(6.)).rounded_full().bg(rgb(
                                if container.running() {
                                    ui::colors().success
                                } else {
                                    ui::colors().muted
                                },
                            )))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(container.name().to_owned()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(ui::colors().muted))
                                    .child(container.state.clone()),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().muted))
                            .child(container.image.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(rgb(ui::colors().muted))
                            .child({
                                let ports = container.ports();
                                if ports.is_empty() {
                                    container.status.clone()
                                } else {
                                    format!("{} · {ports}", container.status)
                                }
                            }),
                    )
                    .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(detail.clone())).into())
                    .on_click(cx.listener(move |this, _, _, cx| this.select(Some(id.clone()), cx)))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                            this.select(Some(key_id.clone()), cx);
                            cx.stop_propagation();
                        }
                    }))
                    .into_any_element()
            }
        }
    }
    fn container(&self, cx: &mut Context<Self>) -> AnyElement {
        let name = self
            .details
            .as_ref()
            .map(|d| d.name.trim_start_matches('/').to_owned())
            .or_else(|| {
                self.containers
                    .iter()
                    .find(|c| Some(&c.id) == self.selected.as_ref())
                    .map(|c| c.name().to_owned())
            })
            .unwrap_or_default();
        let enabled = self.interactive(cx) && self.action_task.is_none();
        let running = self.details.as_ref().is_some_and(|d| d.state.running);
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        ui::icon_button("docker-back", Icon::ChevronLeft, key::DOCKER_BACK)
                            .on_click(cx.listener(|this, _, _, cx| this.select(None, cx))),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name)),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        [Action::Start, Action::Stop, Action::Restart]
                            .into_iter()
                            .enumerate()
                            .map(|(i, action)| {
                                ui::text_button_enabled(
                                    ("docker-action", i),
                                    action_label(action),
                                    ButtonStyle::Neutral,
                                    enabled
                                        && self.details.is_some()
                                        && if action == Action::Start {
                                            !running
                                        } else {
                                            running
                                        },
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| this.action(action, window, cx),
                                ))
                            }),
                    )
                    .child(
                        ui::text_button_enabled(
                            "docker-console",
                            key::DOCKER_CONSOLE,
                            ButtonStyle::Primary,
                            enabled && running,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.console(cx))),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(muted(tr(key::DOCKER_SHELL)))
                    .children([Shell::Sh, Shell::Bash].into_iter().enumerate().map(
                        |(i, shell)| {
                            ui::text_button_enabled(
                                ("docker-shell", i),
                                shell.path(),
                                if self.shell == shell {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                },
                                enabled,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.interactive(cx) {
                                        this.shell = shell;
                                        cx.notify();
                                    }
                                },
                            ))
                        },
                    )),
            )
            .child(
                div()
                    .px_3()
                    .flex()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
                    .children(
                        [
                            (Page::Details, key::DOCKER_OVERVIEW),
                            (Page::Logs, key::DOCKER_LOGS),
                            (Page::Stats, key::DOCKER_STATS),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (page, label))| {
                            ui::text_button_enabled(
                                ("docker-page", i),
                                label,
                                if self.page == page {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                },
                                enabled,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.page(page, cx)))
                        }),
                    ),
            );
        body = body.child(match self.page {
            Page::Details => self.details(),
            Page::Logs => self.log_view(cx),
            Page::Stats => self.statistics(),
        });
        body.into_any_element()
    }
    fn details(&self) -> AnyElement {
        let mut body = div()
            .id("docker-details")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_3();
        if let Some(d) = &self.details {
            let command = d
                .config
                .entrypoint
                .iter()
                .flatten()
                .chain(d.config.cmd.iter().flatten())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            for (label, value) in [
                ("ID", d.id.clone()),
                (key::DOCKER_IMAGE, d.config.image.clone()),
                (
                    key::DOCKER_STATE,
                    if d.state.running {
                        d.state.status.clone()
                    } else {
                        format!("{} · exit {}", d.state.status, d.state.exit_code)
                    },
                ),
                (
                    key::DOCKER_HEALTH,
                    d.state
                        .health
                        .as_ref()
                        .map(|h| h.status.clone())
                        .unwrap_or_else(|| "—".into()),
                ),
                (key::DOCKER_CREATED, timestamp(&d.created)),
                (key::DOCKER_STARTED, timestamp(&d.state.started_at)),
                (key::DOCKER_COMMAND, command),
                (
                    key::DOCKER_USER,
                    if d.config.user.is_empty() {
                        tr(key::DOCKER_DEFAULT_USER).into()
                    } else {
                        d.config.user.clone()
                    },
                ),
                (key::DOCKER_WORKDIR, d.config.working_dir.clone()),
                (
                    key::DOCKER_RESTART_POLICY,
                    d.host_config.restart_policy.name.clone(),
                ),
                (
                    key::DOCKER_PORTS,
                    self.containers
                        .iter()
                        .find(|c| c.id == d.id)
                        .map(Container::ports)
                        .unwrap_or_default(),
                ),
            ] {
                body = body.child(field(label, value));
            }
            body = body.child(field(
                key::DOCKER_MOUNTS,
                d.mounts
                    .iter()
                    .map(|m| {
                        format!(
                            "{} → {} ({}, {})",
                            m.source,
                            m.destination,
                            m.kind,
                            if m.writable { "rw" } else { "ro" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            body = body.child(field(
                key::DOCKER_NETWORKS,
                d.network_settings
                    .networks
                    .iter()
                    .flat_map(|n| n.iter())
                    .map(|(name, n)| format!("{name}: {} {}", n.address, n.ipv6))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            if !d.state.error.is_empty() {
                body = body.child(
                    div()
                        .text_color(rgb(ui::colors().danger))
                        .child(d.state.error.clone()),
                );
            }
        } else {
            body = body.child(tr(if self.failed {
                key::DOCKER_UNAVAILABLE
            } else {
                key::DOCKER_LOADING
            }));
        }
        ui::scrollable(body, None).into_any_element()
    }
    fn statistics(&self) -> AnyElement {
        if self
            .details
            .as_ref()
            .is_some_and(|details| details.state.running)
            && let Some(stats) = &self.stats
        {
            return stats.view.clone().into_any_element();
        }
        div()
            .p_4()
            .child(tr(if self.failed {
                key::DOCKER_UNAVAILABLE
            } else if self
                .details
                .as_ref()
                .is_some_and(|details| details.state.running)
            {
                key::DOCKER_LOADING
            } else {
                key::DOCKER_NO_STATS
            }))
            .into_any_element()
    }
    fn log_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.log_rows.clone();
        let width = self.log_width;
        let count = rows.len();
        let list = uniform_list(
            "docker-log-lines",
            count,
            cx.processor(move |_, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|i| {
                        div()
                            .h(px(22.))
                            .px_3()
                            .text_size(px(12.))
                            .font_family(ui::MONOSPACE_FONT)
                            .text_color(rgb(if rows[i].stderr {
                                ui::colors().warning
                            } else {
                                ui::colors().text
                            }))
                            .child(rows[i].text.clone())
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.log_scroll.clone())
        .w(px(width))
        .h_full();
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.log_search.clone())
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                ui::text_button(
                                    "docker-log-pause",
                                    if self.log_paused {
                                        key::DOCKER_RESUME
                                    } else {
                                        key::DOCKER_PAUSE
                                    },
                                    ButtonStyle::Neutral,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_logs(cx))),
                            )
                            .child(
                                ui::text_button(
                                    "docker-log-copy",
                                    key::DOCKER_COPY,
                                    ButtonStyle::Neutral,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if this.interactive(cx) {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                this.logs.text(this.log_search.read(cx).value()),
                                            ));
                                        }
                                    },
                                )),
                            )
                            .child(
                                ui::icon_button(
                                    "docker-log-bottom",
                                    Icon::ChevronDown,
                                    key::DOCKER_SCROLL_BOTTOM,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, _| {
                                        this.log_follow = true;
                                        this.log_scroll.scroll_to_item(
                                            count.saturating_sub(1),
                                            gpui::ScrollStrategy::Bottom,
                                        )
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .pb(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        div()
                            .id("docker-log-horizontal")
                            .track_scroll(&self.log_horizontal)
                            .on_scroll_wheel(cx.listener(|this, _, _, _| this.log_follow = false))
                            .size_full()
                            .overflow_x_scroll()
                            .map(ui::preserve_scroll_axes)
                            .child(list),
                    )
                    .child(ui::scrollbars(
                        "docker-log-bars",
                        Some(self.log_horizontal.clone()),
                        Some(self.log_scroll.0.borrow().base_handle.clone()),
                    )),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(key::DOCKER_LOG_RETENTION)),
            );
        if let Some(error) = &self.log_error {
            body = body.child(
                div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(ui::colors().danger))
                    .child(error.clone()),
            );
        } else if self.log_ended {
            body = body.child(div().px_3().py_2().child(tr(key::DOCKER_LOG_ENDED)));
        }
        body.into_any_element()
    }
}
fn muted(value: impl Into<gpui::SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(ui::colors().muted))
        .child(value.into())
}
fn field(label: &str, value: String) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(muted(tr(label).to_owned()))
        .child(if value.is_empty() {
            "—".into()
        } else {
            value
        })
}
fn timestamp(value: &str) -> String {
    use chrono::Datelike;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .filter(|time| time.year() > 1)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}
