use super::{TerminalKind, TerminalView};
use super::{chrome::pane_button, paint::paint_terminal};
use crate::app::{
    pane::PaneEvent,
    ui::{self, Icon},
};
use gpui::{
    Context, CursorStyle, ElementInputHandler, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Window, canvas, div, prelude::*, px, rgb,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use rustterm::session::Command;
use rustterm::terminal::Snapshot;

fn needs_notice(error: bool, snapshot: Option<&Snapshot>) -> bool {
    error
        || snapshot.is_some_and(|s| {
            s.shell.command_insert_blocked
                || s.shell.command_unavailable
                || s.viewport_evicted
                || s.selection_invalidated
        })
}

fn history_action(snapshot: &Snapshot) -> Option<&'static str> {
    (snapshot.display_offset > 0 && !snapshot.alternate_screen).then_some(if snapshot.new_output {
        i18n_key::TERMINAL_NEW_OUTPUT
    } else {
        i18n_key::TERMINAL_SCROLL_TO_BOTTOM
    })
}

impl TerminalView {
    pub fn external_header(&mut self, cx: &mut Context<Self>) -> gpui::Entity<TerminalHeader> {
        if let Some(header) = &self.external_header {
            return header.clone();
        }
        let owner = cx.entity();
        let header = cx.new(|cx| TerminalHeader {
            terminal: owner.downgrade(),
            focus: cx.focus_handle(),
            _observe: cx.observe(&owner, |_, _, cx| cx.notify()),
        });
        self.external_header = Some(header.clone());
        header
    }

    pub fn header_owns_focus(&self, window: &Window, cx: &gpui::App) -> bool {
        self.header_external
            && self
                .external_header
                .as_ref()
                .is_some_and(|header| header.read(cx).focus.contains_focused(window, cx))
    }

    pub(super) fn reset_search(&mut self, cx: &mut Context<Self>) {
        self.close_search(cx);
        self.search_input.update(cx, |input, cx| input.clear(cx));
    }

    pub(super) fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search_seed_task.take();
        self.search_open = false;
        self.search_scope = Default::default();
        self.send(Command::CancelSearch, cx);
    }

    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_final_alternate {
            self.toggle_final_alternate(cx);
        }
        if !self.search_open {
            let query = self.search_input.read(cx).value().to_owned();
            self.send(
                Command::SearchScoped {
                    query,
                    scope: self.search_scope,
                    case_sensitive: self.search_case,
                },
                cx,
            );
            self.seed_search(cx);
        }
        self.search_open = true;
        cx.emit(PaneEvent::Focused(self.index));
        self.search_input
            .update(cx, |input, cx| input.select_all(cx));
        self.search_input.read(cx).focus_handle().focus(window);
        cx.notify();
    }

    fn seed_search(&mut self, cx: &mut Context<Self>) {
        self.search_seed_task.take();
        if !self.flush_motion(cx) {
            return;
        }
        let Some(run) = self.runs.get(self.viewed_run) else {
            return;
        };
        let Ok(receipt) = run.selection_search_query() else {
            return;
        };
        let run_id = run.id();
        let generation = self.generation;
        let lock_epoch = self.window_state.prompts.lock_epoch();
        let previous_query = self.search_input.read(cx).value().to_owned();
        self.search_seed_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Some(query)) = receipt.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if !this.search_open
                    || !this.visible
                    || this.chrome_blocked(cx)
                    || this.generation != generation
                    || this.window_state.prompts.lock_epoch() != lock_epoch
                    || this.search_input.read(cx).value() != previous_query
                    || !this
                        .runs
                        .get(this.viewed_run)
                        .is_some_and(|run| run.id() == run_id && !run.is_evicted())
                {
                    return;
                }
                this.search_input.update(cx, |input, cx| {
                    input.set_value(&query, cx);
                    input.select_all(cx);
                });
                this.send(
                    Command::SearchScoped {
                        query,
                        scope: this.search_scope,
                        case_sensitive: this.search_case,
                    },
                    cx,
                );
                cx.notify();
            });
        }));
    }

    fn history_scrollbar(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let scrollbar_terminal = cx.entity().downgrade();
        let scrollbar_metrics = scrollbar_terminal.clone();
        let markers = self.snapshot.as_ref().filter(|snapshot| {
            self.search_open
                && !self.show_final_alternate
                && !snapshot.search_markers.rows.is_empty()
                && snapshot.search.scope == self.search_scope
                && snapshot.search.case_sensitive == self.search_case
                && snapshot.search.query == self.search_input.read(cx).value()
        });
        ui::scrollbar(
            ("terminal-history-scrollbar", self.index),
            ui::ScrollAxis::Vertical,
            move |cx| {
                scrollbar_metrics
                    .upgrade()
                    .map(|terminal| {
                        let terminal = terminal.read(cx);
                        terminal
                            .snapshot
                            .as_ref()
                            .filter(|snapshot| {
                                !terminal.show_final_alternate && !snapshot.alternate_screen
                            })
                            .map(|snapshot| ui::ScrollMetrics {
                                viewport: f32::from(snapshot.rows),
                                content: snapshot.history_size as f32 + f32::from(snapshot.rows),
                                offset: snapshot
                                    .history_size
                                    .saturating_sub(snapshot.display_offset)
                                    as f32
                                    + snapshot.scroll_fraction,
                            })
                            .unwrap_or_default()
                    })
                    .unwrap_or_default()
            },
            move |offset, _, cx| {
                let _ = scrollbar_terminal.update(cx, |terminal, cx| {
                    let Some(snapshot) = terminal.snapshot.as_ref().filter(|snapshot| {
                        !terminal.show_final_alternate && !snapshot.alternate_screen
                    }) else {
                        return;
                    };
                    let offset = snapshot
                        .history_size
                        .saturating_sub(offset.round() as usize);
                    terminal.send(Command::ScrollTo(offset), cx);
                });
            },
        )
        .when_some(markers, |bar, snapshot| {
            let marks = &snapshot.search_markers;
            bar.with_vertical_markers(marks.rows.clone(), marks.current)
        })
    }

    fn search_status_label(&self) -> String {
        let Some(snapshot) = &self.snapshot else {
            return tr(i18n_key::TERMINAL_WAITING_FOR_TERMINAL).into();
        };
        let status = &snapshot.search;
        if status.scope_unavailable {
            return tr(i18n_key::TERMINAL_SEARCH_COMMAND_EXPIRED).into();
        }
        if status.paused {
            return tr(i18n_key::TERMINAL_SEARCH_PAUSED_IN_ALTERNATE_SCREEN).into();
        }
        if status.query.is_empty() {
            return tr(i18n_key::TERMINAL_ENTER_A_QUERY).into();
        }
        if status.current_unavailable {
            return tr(i18n_key::TERMINAL_SEARCH_MATCH_UNAVAILABLE).into();
        }
        if !status.complete {
            return rustterm::trf!(
                i18n_key::TERMINAL_SEARCH_PROGRESS,
                scanned = status.scanned_rows,
                total = status.total_rows,
                matches = status.matches
            );
        }
        let position = status
            .current
            .map(|n| (n + 1).to_string())
            .unwrap_or_else(|| "—".into());
        format!(
            "{position}/{}{}",
            status.matches,
            if status.truncated {
                tr(i18n_key::TERMINAL_STATUS_RESULTS_TRUNCATED)
            } else {
                ""
            }
        )
    }
}

impl TerminalView {
    fn header_notice(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !needs_notice(self.error.is_some(), self.snapshot.as_deref()) {
            return None;
        }
        let text = self
            .feedback_text()
            .filter(|text| self.dismissed_feedback.as_ref() != Some(text))?;
        Some(
            div()
                .flex()
                .min_w_0()
                .max_w(gpui::relative(0.4))
                .items_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    ui::icon_button(
                        ("feedback-details", self.index),
                        Icon::Warning,
                        i18n_key::TERMINAL_CONNECTION_AND_STATUS_DETAILS,
                    )
                    .when(!self.compact_header, |button| {
                        button
                            .w_auto()
                            .min_w_0()
                            .flex_shrink()
                            .px_1()
                            .gap_1()
                            .text_size(px(11.))
                            .text_color(rgb(self.status_color()))
                            .child(div().min_w_0().truncate().child(text))
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.show_feedback(window, cx))),
                )
                .child(
                    pane_button("feedback-dismiss", self.index, Icon::Close).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.dismissed_feedback = this.feedback_text();
                            cx.notify();
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    pub fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let focused = self.focus.is_focused(window)
            || self.menu_focus.is_focused(window)
            || (self.search_open && self.search_input.read(cx).focus_handle().is_focused(window));
        let pane_index = self.index;
        let header_entity = cx.entity();
        let address = if self.kind != TerminalKind::Local {
            self.target.as_ref().unwrap_or(&self.shell)
        } else {
            self.local_shell.as_ref().unwrap_or(&self.shell)
        }
        .clone();
        let mut detail = self.display_name.clone();
        if self.production_protected() {
            detail.push('\n');
            detail.push_str(tr(i18n_key::CONNECTIONS_PRODUCTION_DESCRIPTION));
        }
        if self.protection_pending() {
            detail.push('\n');
            detail.push_str(tr(i18n_key::CONNECTIONS_PRODUCTION_PENDING));
        }
        if self.kind == TerminalKind::Local
            && let Some(directory) = &self.target
        {
            detail.push('\n');
            detail.push_str(directory);
        }
        ui::panel_header()
            .relative()
            .min_w_0()
            .overflow_hidden()
            .child(
                ui::connection_address(
                    ("terminal-address", pane_index),
                    match self.kind {
                        TerminalKind::Container => Icon::Container,
                        TerminalKind::Local => Icon::Local,
                        TerminalKind::Ssh => Icon::Ssh,
                        TerminalKind::Telnet => Icon::Telnet,
                    },
                    match self.kind {
                        TerminalKind::Local => tr(i18n_key::WORKSPACE_LOCAL),
                        TerminalKind::Ssh => "SSH",
                        TerminalKind::Telnet => "Telnet",
                        TerminalKind::Container => "Docker",
                    },
                    address,
                    detail,
                )
                .flex_1()
                .min_w_0(),
            )
            .when(self.production_protected(), |header| {
                header.child(ui::production_badge())
            })
            .when(self.protection_pending(), |header| {
                header.child(ui::protection_pending_badge())
            })
            .children(self.header_notice(cx))
            .when_some(
                self.snapshot
                    .as_deref()
                    .and_then(history_action)
                    .filter(|_| !self.show_final_alternate),
                |header, label| {
                    header.child(
                        ui::icon_button_enabled(
                            ("bottom", pane_index),
                            Icon::ChevronDown,
                            if label == i18n_key::TERMINAL_NEW_OUTPUT {
                                label
                            } else {
                                i18n_key::TERMINAL_SCROLL_TO_BOTTOM_AND_RESUME_FOLLOWING
                            },
                            self.visible && !self.chrome_blocked(cx),
                        )
                        .when(label == i18n_key::TERMINAL_NEW_OUTPUT, |button| {
                            button.bg(rgb(ui::colors().selected))
                        })
                        .when(!self.compact_header, |button| {
                            button
                                .w_auto()
                                .px_2()
                                .gap_1()
                                .text_size(px(11.))
                                .child(tr(label))
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.visible && !this.chrome_blocked(cx) {
                                this.send(Command::ScrollToBottom, cx);
                            }
                        })),
                    )
                },
            )
            .child(self.header_actions(cx))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        header_entity.update(cx, |this, cx| {
                            if this.header_bounds != Some(bounds) {
                                this.header_bounds = Some(bounds);
                                if this.menu_open {
                                    cx.notify();
                                }
                            }
                            let threshold = 400.
                                + if this.viewed_run + 1 < this.runs.len() {
                                    60.
                                } else {
                                    0.
                                };
                            let compact = f32::from(bounds.size.width) < threshold;
                            if this.compact_header != compact {
                                this.compact_header = compact;
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .when(focused, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(rgb(ui::colors().accent)),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.visible && !window.default_prevented() && !this.chrome_blocked(cx) {
                        this.focus.focus(window);
                    }
                }),
            )
            .into_any_element()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let pane_index = self.index;
        let mut body = div()
            .id(("terminal-body", self.index))
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden();
        if self.visible && self.window_state.visibility.is_visible() {
            body = body
                .pr(px(ui::SCROLLBAR_WIDTH))
                .when_some(self.hovered_link.as_ref(), |body, link| {
                    let target = match link {
                        rustterm::terminal::TerminalLink::Url(value)
                        | rustterm::terminal::TerminalLink::RemotePath(value) => {
                            value.chars().take(512).collect::<String>()
                        }
                    };
                    let hint = rustterm::trf!(i18n_key::TERMINAL_OPEN_LINK_HINT, target = target);
                    body.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(hint.clone())).into())
                })
                .cursor(if self.hovered_link.is_some() {
                    CursorStyle::PointingHand
                } else {
                    CursorStyle::IBeam
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        let intent = super::input::selection_intent(
                            this.focus.is_focused(window),
                            this.selection_contains(event.position),
                            event.click_count,
                            event.modifiers,
                        );
                        this.focus.focus(window);
                        if this.show_final_alternate {
                            return;
                        }
                        if this.begin_link(event.position, event.modifiers, cx) {
                            window.prevent_default();
                            cx.stop_propagation();
                            return;
                        }
                        let kind = if intent == super::input::SelectionIntent::Preserve {
                            Some(rustterm::terminal::SelectionKind::Character)
                        } else {
                            super::input::local_selection_kind(
                                event.click_count,
                                event.modifiers,
                                this.snapshot.as_ref().is_some_and(|s| s.mouse_reporting),
                                this.viewed_run + 1 < this.runs.len(),
                            )
                        };
                        if let Some(kind) = kind {
                            this.begin_selection_with_intent(event.position, kind, intent, cx);
                            window.prevent_default();
                            cx.stop_propagation();
                        } else {
                            this.mouse(event.position, 0, true, false, event.modifiers, cx);
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 0, false, false, event.modifiers, cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        if !event.modifiers.shift
                            && !this.show_final_alternate
                            && this.viewed_run + 1 == this.runs.len()
                            && this.snapshot.as_ref().is_some_and(|s| s.mouse_reporting)
                        {
                            this.focus.focus(window);
                            this.mouse(event.position, 2, true, false, event.modifiers, cx);
                        } else {
                            this.open_context_menu(event.position, window, cx);
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 2, false, false, event.modifiers, cx)
                    }),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.focus.focus(window);
                        if event.modifiers.shift
                            || !this.snapshot.as_ref().is_some_and(|s| s.mouse_reporting)
                        {
                            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                            this.paste_primary(window, cx);
                        } else {
                            this.mouse(event.position, 1, true, false, event.modifiers, cx);
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 1, false, false, event.modifiers, cx)
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 0, false, false, event.modifiers, cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Middle,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 1, false, false, event.modifiers, cx)
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.mouse(event.position, 2, false, false, event.modifiers, cx)
                    }),
                )
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    let link = if super::links::link_modifier(event.modifiers)
                        && event.pressed_button.is_none()
                    {
                        this.link_at_position(event.position, cx)
                    } else {
                        None
                    };
                    if this.hovered_link != link {
                        this.hovered_link = link;
                        cx.notify();
                    }
                    let button = match event.pressed_button {
                        Some(MouseButton::Left) => 0,
                        Some(MouseButton::Middle) => 1,
                        Some(MouseButton::Right) => 2,
                        _ => 3,
                    };
                    this.mouse(
                        event.position,
                        button,
                        event.pressed_button.is_some(),
                        true,
                        event.modifiers,
                        cx,
                    );
                }))
                .on_scroll_wheel(cx.listener(Self::scroll))
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            entity.update(cx, |this, cx| this.layout_canvas(bounds, window, cx));
                        },
                        move |bounds, _, window, cx| {
                            paint_entity.update(cx, |this, cx| {
                                this.capture_selection_input(window, cx);
                                window.handle_input(
                                    &this.focus,
                                    ElementInputHandler::new(bounds, paint_entity.clone()),
                                    cx,
                                );
                                paint_terminal(this, bounds, window, cx);
                            });
                        },
                    )
                    .size_full(),
                )
                .child(self.history_scrollbar(cx))
                .children(self.render_context_menu(cx).map(gpui::deferred));
            body = body
                .children(self.render_command_guides(cx))
                .children(self.render_command_results())
                .children(self.render_sticky_command(cx));
        } else {
            body = body
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(
                    super::colors::OverlayColors::new(&self.settings.theme).muted
                ))
                .text_size(px(12.))
                .child(tr(
                    i18n_key::TERMINAL_THE_TERMINAL_IS_RUNNING_IN_THE_BACKGROUND,
                ));
        }
        let root = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .overflow_hidden()
            .bg(rgb(self.settings.theme.background))
            .text_color(rgb(ui::colors().text))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::search_key))
            .on_key_down(cx.listener(Self::key_down))
            .on_modifiers_changed(
                cx.listener(|this, event: &gpui::ModifiersChangedEvent, _, _| {
                    if !super::links::link_modifier(event.modifiers) {
                        this.link_press = None;
                    }
                }),
            )
            .when(!self.header_external, |root| {
                root.child(self.render_header(window, cx))
            })
            .children(self.render_transfer(cx))
            .when(self.search_open, |d| {
                d.child(
                    div()
                        .h(px(ui::SEARCH_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(8.))
                        .bg(rgb(ui::colors().raised))
                        .border_b_1()
                        .border_color(rgb(ui::colors().border))
                        .child(
                            ui::icon(Icon::Search)
                                .size(px(14.))
                                .text_color(rgb(ui::colors().muted)),
                        )
                        .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                        .child(
                            div()
                                .max_w(px(150.))
                                .truncate()
                                .text_color(rgb(ui::colors().muted))
                                .text_size(px(10.))
                                .child(self.search_status_label()),
                        )
                        .child(self.render_search_scope(cx))
                        .child(
                            div()
                                .id(("case", pane_index))
                                .tab_index(0)
                                .h(px(24.))
                                .min_w(px(28.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(rgb(if self.search_case {
                                    ui::colors().accent
                                } else {
                                    ui::colors().muted
                                }))
                                .when(self.search_case, |d| d.bg(rgb(ui::colors().surface)))
                                .hover(|s| {
                                    s.bg(rgb(ui::colors().surface))
                                        .text_color(rgb(ui::colors().text))
                                })
                                .tooltip(|_, cx| {
                                    cx.new(|_| {
                                        ui::Tooltip(tr(i18n_key::TERMINAL_MATCH_CASE).to_owned())
                                    })
                                    .into()
                                })
                                .child("Aa")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.search_case = !this.search_case;
                                    let query = this.search_input.read(cx).value().to_owned();
                                    this.send(
                                        Command::SearchScoped {
                                            query,
                                            scope: this.search_scope,
                                            case_sensitive: this.search_case,
                                        },
                                        cx,
                                    );
                                    cx.notify();
                                })),
                        )
                        .child(
                            pane_button("prev", pane_index, Icon::ChevronUp)
                                .tab_index(0)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send(Command::NextMatch { backwards: true }, cx)
                                })),
                        )
                        .child(
                            pane_button("next", pane_index, Icon::ChevronDown)
                                .tab_index(0)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send(Command::NextMatch { backwards: false }, cx)
                                })),
                        )
                        .child(
                            pane_button("search-close", pane_index, Icon::Close)
                                .tab_index(0)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_search(cx);
                                    this.focus.focus(window);
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(body);
        root.when(!self.header_external, |root| {
            root.children(self.render_menu(cx))
        })
        .children(self.render_paste_preview(cx))
    }
}

pub struct TerminalHeader {
    terminal: gpui::WeakEntity<TerminalView>,
    focus: gpui::FocusHandle,
    _observe: gpui::Subscription,
}

impl Render for TerminalHeader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self
            .terminal
            .update(cx, |terminal, cx| {
                (terminal.render_header(window, cx), terminal.render_menu(cx))
            })
            .ok();
        div()
            .track_focus(&self.focus)
            .relative()
            .h(px(ui::PANE_HEADER_HEIGHT))
            .flex_shrink_0()
            .w_full()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let _ = this
                    .terminal
                    .update(cx, |terminal, cx| terminal.key_down(event, window, cx));
            }))
            .when_some(content, |root, (header, menu)| {
                root.child(header).children(menu.map(gpui::deferred))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::terminal::{SearchHighlight, Terminal};

    #[test]
    fn navigating_to_a_match_on_the_last_row_keeps_history_out_of_canvas_notices() {
        let mut terminal = Terminal::new(12, 3, 32);
        terminal.feed(b"match 0\r\nmatch 1\r\nmatch 2\r\nmatch 3\r\nmatch 4\r\nmatch 5");
        terminal.scroll_to(2);
        terminal.start_search("match".into(), true);
        assert!(terminal.search_step(32).complete);
        assert!(terminal.search_next(true));
        assert!(terminal.search_next(true));
        assert!(terminal.search_next(false));
        for output in [b"".as_slice(), b"\r\nfresh"] {
            terminal.feed(output);
            let snapshot = terminal.snapshot();
            assert!(snapshot.display_offset > 0);
            assert_eq!(snapshot.new_output, !output.is_empty());
            assert_eq!(
                snapshot.lines[2].cells[0].search_highlight,
                Some(SearchHighlight::Current)
            );
            assert!(!needs_notice(false, Some(&snapshot)));
            assert_eq!(
                history_action(&snapshot),
                Some(if output.is_empty() {
                    i18n_key::TERMINAL_SCROLL_TO_BOTTOM
                } else {
                    i18n_key::TERMINAL_NEW_OUTPUT
                })
            );
        }
        terminal.scroll_to_bottom();
        assert!(history_action(&terminal.snapshot()).is_none());
    }
}
