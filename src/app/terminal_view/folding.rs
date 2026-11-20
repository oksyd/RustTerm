use super::{PADDING, TerminalView, colors::OverlayColors};
use crate::app::ui::{self, Icon};
use gpui::{
    AnyElement, ClickEvent, Context, Div, MouseButton, Pixels, Point, div, prelude::*, px, rgb,
};
use rustterm::{
    i18n::messages as i18n_key,
    session::Command,
    terminal::{CommandBlock, CommandHeading, Snapshot},
};

pub(super) const GUTTER_COLUMNS: usize = 2;

fn block_at_row(snapshot: &Snapshot, row: usize) -> Option<u64> {
    snapshot
        .command_blocks
        .iter()
        .find(|block| block.rows.contains(&row))
        .map(|block| block.marker.id)
}

pub(super) fn keyboard_block(snapshot: &Snapshot) -> Option<u64> {
    let mut selected = snapshot
        .command_blocks
        .iter()
        .filter(|block| block.selected);
    if let Some(block) = selected.next() {
        return selected.next().is_none().then_some(block.marker.id);
    }
    if snapshot.has_selection {
        return None;
    }
    block_at_row(snapshot, snapshot.cursor.row).or_else(|| {
        snapshot
            .command_blocks
            .iter()
            .rev()
            .find(|block| block.rows.end <= snapshot.cursor.row)
            .map(|block| block.marker.id)
    })
}

fn result_label(result: &rustterm::terminal::CommandResult) -> Option<String> {
    use rustterm::terminal::CommandResult;
    match result {
        CommandResult::Running => {
            Some(rustterm::i18n::tr(i18n_key::TERMINAL_BLOCK_RUNNING).to_owned())
        }
        CommandResult::Finished {
            exit_status,
            duration,
        } => Some(rustterm::trf!(
            i18n_key::TERMINAL_BLOCK_RESULT,
            status = exit_status.map_or_else(|| "?".into(), |code| code.to_string()),
            duration = format!("{:.1}", duration.as_secs_f64())
        )),
        CommandResult::Unknown => None,
    }
}

fn inline_result_label(result: &rustterm::terminal::CommandResult) -> Option<String> {
    match result {
        rustterm::terminal::CommandResult::Finished {
            exit_status: Some(0),
            duration,
        } if *duration < super::completion::LONG_COMMAND_DURATION => None,
        _ => result_label(result),
    }
}

fn guide_extent(block: &CommandBlock, height: f32) -> Option<(f32, f32)> {
    let start = if block.header.is_some() {
        height * 0.5 + height.min(12.) * 0.5
    } else {
        0.
    };
    let end = height * block.rows.len() as f32 - if block.ends_here { height * 0.5 } else { 0. };
    (end > start).then_some((start, end))
}

fn guide_stroke(color: u32, accent: u32) -> Div {
    div()
        .absolute()
        .bg(rgb(color))
        .group_hover("command-guide", move |style| style.bg(rgb(accent)))
}

fn visible_command_heading(
    snapshot: &Snapshot,
    search_open: bool,
    final_alternate: bool,
) -> Option<&CommandHeading> {
    snapshot
        .sticky_command
        .as_ref()
        .filter(|_| !search_open && !final_alternate && snapshot.rows >= 2)
}

impl TerminalView {
    pub(super) fn render_sticky_command(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let heading = visible_command_heading(
            self.snapshot.as_deref()?,
            self.search_open,
            self.show_final_alternate,
        )?;
        let id = heading.id;
        Some(
            div()
                .id(("sticky-command", self.index))
                .absolute()
                .occlude()
                .top_0()
                .left_0()
                .right(px(12.))
                .h(self.line_height + px(4.))
                .px(px(PADDING))
                .flex()
                .items_center()
                .gap(px(6.))
                .bg(rgb(ui::colors().raised))
                .border_b_1()
                .border_color(rgb(ui::colors().border))
                .text_size(px(11.))
                .cursor_pointer()
                .child(ui::icon(Icon::ChevronUp).size(px(12.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(heading.text.clone()),
                )
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        ui::Tooltip(rustterm::i18n::tr(i18n_key::TERMINAL_RETURN_TO_COMMAND).into())
                    })
                    .into()
                })
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send(Command::RevealCommandBlock { id }, cx);
                    cx.stop_propagation();
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_search_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        use rustterm::terminal::SearchScope;
        let current = matches!(self.search_scope, SearchScope::Command(_));
        let candidate = self.snapshot.as_ref().and_then(|s| {
            if s.has_selection {
                keyboard_block(s)
            } else {
                s.sticky_command
                    .as_ref()
                    .map(|heading| heading.id)
                    .or_else(|| keyboard_block(s))
                    .or_else(|| s.command_blocks.first().map(|b| b.marker.id))
            }
        });
        let next_scope = if current {
            Some(SearchScope::Session)
        } else {
            candidate.map(SearchScope::Command)
        };
        let enabled = next_scope.is_some();
        ui::text_button(
            ("search-scope", self.index),
            if current {
                i18n_key::TERMINAL_SEARCH_COMMAND
            } else {
                i18n_key::TERMINAL_SEARCH_SESSION
            },
            ui::ButtonStyle::Neutral,
        )
        .when(enabled, |d| d.tab_index(0))
        .tooltip(|_, cx| {
            cx.new(|_| ui::Tooltip(rustterm::i18n::tr(i18n_key::TERMINAL_SEARCH_SCOPE_HELP).into()))
                .into()
        })
        .h(px(24.))
        .opacity(if enabled { 1. } else { 0.45 })
        .when_some(next_scope, |d, scope| {
            d.on_click(cx.listener(move |this, _, _, cx| {
                this.search_scope = scope;
                this.send(
                    Command::SearchScoped {
                        query: this.search_input.read(cx).value().to_owned(),
                        case_sensitive: this.search_case,
                        scope: this.search_scope,
                    },
                    cx,
                );
                cx.notify();
            }))
        })
        .into_any_element()
    }

    fn fold_gutter_left(&self) -> Pixels {
        px(PADDING) + self.gutter_width() - self.cell_width * GUTTER_COLUMNS as f32
    }

    pub(super) fn block_at_position(&self, position: Point<Pixels>) -> Option<u64> {
        if self.show_final_alternate {
            return None;
        }
        let bounds = self.bounds?;
        if !bounds.contains(&position) || position.x < bounds.left() + self.fold_gutter_left() {
            return None;
        }
        let y = f32::from(position.y - bounds.top() - px(PADDING) + self.scroll_offset());
        if y < 0. {
            return None;
        }
        block_at_row(
            self.snapshot.as_ref()?,
            (y / f32::from(self.line_height)).floor() as usize,
        )
    }

    pub(super) fn select_block(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.chrome_blocked(cx) || self.show_final_alternate {
            return;
        }
        self.selection_drag = None;
        self.selection_pointer = None;
        self.primary_copy_task.take();
        self.send(Command::SelectCommandBlock { id }, cx);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        self.copy_primary(cx);
    }

    pub(super) fn render_command_guides(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.show_final_alternate {
            return Vec::new();
        }
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let palette = OverlayColors::new(&self.settings.theme);
        snapshot
            .command_blocks
            .iter()
            .map(|block| {
                let marker = block.marker;
                let color = if block.selected {
                    palette.accent
                } else {
                    palette.muted
                };
                let width = self.cell_width * GUTTER_COLUMNS as f32;
                let mut guide = div()
                    .id(("command-guide", marker.id))
                    .group("command-guide")
                    .absolute()
                    .left(self.fold_gutter_left())
                    .top(
                        px(PADDING) + self.line_height * block.rows.start as f32
                            - self.scroll_offset(),
                    )
                    .w(width)
                    .h(self.line_height * block.rows.len() as f32)
                    .cursor_pointer()
                    .tooltip(|_, cx| {
                        cx.new(|_| {
                            ui::Tooltip(
                                rustterm::i18n::tr(i18n_key::TERMINAL_SELECT_COMMAND_BLOCK_HINT)
                                    .to_owned(),
                            )
                        })
                        .into()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.focus.focus(window);
                            window.prevent_default();
                            cx.stop_propagation();
                        }),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if event.click_count() == 2 {
                            this.select_block(marker.id, cx);
                        }
                        cx.stop_propagation();
                    }));
                if let Some((start, end)) = guide_extent(block, f32::from(self.line_height)) {
                    guide = guide.child(
                        guide_stroke(color, palette.accent)
                            .left(width * 0.5 - px(0.5))
                            .top(px(start))
                            .w(px(1.))
                            .h(px(end - start)),
                    );
                    if block.ends_here {
                        guide = guide.child(
                            guide_stroke(color, palette.accent)
                                .left(width * 0.5 - px(0.5))
                                .top(px(end))
                                .w((width * 0.25).min(px(5.)))
                                .h(px(1.)),
                        );
                    }
                }
                if block.header.is_some() {
                    guide = guide.child(self.render_fold_toggle(block, palette, cx));
                }
                guide.into_any_element()
            })
            .collect()
    }
    pub(super) fn render_command_results(&self) -> Vec<AnyElement> {
        use rustterm::terminal::CommandResult;
        if self.show_final_alternate {
            return Vec::new();
        }
        let palette = OverlayColors::new(&self.settings.theme);
        self.snapshot
            .iter()
            .flat_map(|s| &s.command_blocks)
            .filter_map(|block| {
                let row = block.header?;
                let label = inline_result_label(&block.details.result)?;
                let line = self.snapshot.as_ref()?.lines.get(row)?;
                let occupied = line
                    .cells
                    .iter()
                    .rposition(|cell| !cell.text.trim().is_empty())
                    .map_or(0, |index| index + 1);
                let remaining = self.bounds?.size.width
                    - px(PADDING + 14.)
                    - self.gutter_width()
                    - self.cell_width * occupied as f32;
                if remaining < px(48.) {
                    return None;
                }
                Some(
                    div()
                        .absolute()
                        .right(px(14.))
                        .top(px(PADDING) + self.line_height * row as f32 - self.scroll_offset())
                        .h(self.line_height)
                        .max_w(remaining)
                        .truncate()
                        .line_height(self.line_height)
                        .px(px(4.))
                        .rounded(px(3.))
                        .bg(rgb(self.settings.theme.background))
                        .text_size(px(10.))
                        .text_color(rgb(match block.details.result {
                            CommandResult::Finished {
                                exit_status: Some(0),
                                ..
                            } => palette.success,
                            CommandResult::Finished {
                                exit_status: Some(_),
                                ..
                            } => palette.danger,
                            _ => palette.muted,
                        }))
                        .child(label)
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_fold_toggle(
        &self,
        block: &CommandBlock,
        palette: OverlayColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let marker = block.marker;
        let selected = block.selected;
        let label = if marker.collapsed {
            i18n_key::TERMINAL_EXPAND_COMMAND_OUTPUT
        } else {
            i18n_key::TERMINAL_COLLAPSE_COMMAND_OUTPUT
        };

        let mut hint = result_label(&block.details.result).unwrap_or_default();
        if marker.output_rows > 0 {
            if !hint.is_empty() {
                hint.push('\n');
            }
            hint.push_str(&rustterm::trf!(label, count = marker.output_rows));
        }
        let icon = if marker.output_rows > 0 {
            if marker.collapsed {
                Icon::ChevronRight
            } else {
                Icon::ChevronDown
            }
        } else {
            match block.details.result {
                rustterm::terminal::CommandResult::Finished {
                    exit_status: Some(0),
                    ..
                } => Icon::Check,
                rustterm::terminal::CommandResult::Finished {
                    exit_status: Some(_),
                    ..
                } => Icon::Close,
                _ => Icon::More,
            }
        };
        div()
            .id(("command-fold", marker.id))
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(self.line_height)
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .text_color(rgb(if marker.collapsed || selected {
                palette.accent
            } else {
                palette.muted
            }))
            .group_hover("command-guide", move |style| {
                style.text_color(rgb(palette.accent))
            })
            .hover(move |style| {
                style
                    .bg(rgb(palette.hover))
                    .text_color(rgb(palette.on_hover))
            })
            .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(hint.clone())).into())
            .child(ui::icon(icon).size(self.line_height.min(px(12.))))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window);
                if event.click_count() == 1 && marker.output_rows > 0 {
                    this.send(Command::ToggleCommandFold { id: marker.id }, cx);
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::terminal::{SelectionKind, Terminal};

    #[test]
    fn ordinary_success_keeps_its_details_in_the_gutter() {
        use rustterm::terminal::CommandResult;
        use std::time::Duration;
        for (status, millis, visible) in [
            (Some(0), 0, false),
            (Some(0), 9_999, false),
            (Some(0), 10_000, true),
            (Some(1), 1, true),
        ] {
            let result = CommandResult::Finished {
                exit_status: status,
                duration: Duration::from_millis(millis),
            };
            assert!(result_label(&result).is_some());
            assert_eq!(inline_result_label(&result).is_some(), visible);
        }
    }

    fn terminal() -> Terminal {
        let mut terminal = Terminal::new(32, 10, 100);
        for name in ["first", "second"] {
            terminal.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07{name}\r\n\x1b]133;C\x07one\r\n\r\n\x1b]133;D;0\x07").as_bytes());
        }
        terminal.feed(b"next> ");
        terminal
    }

    #[test]
    fn opening_find_hides_the_command_heading_even_before_a_query_is_entered() {
        let mut terminal = Terminal::new(24, 3, 100);
        terminal.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07one\r\ntwo\r\nthree\r\nfour\r\n",
        );
        let snapshot = terminal.snapshot();
        assert!(snapshot.search.query.is_empty());
        for (search_open, expected) in [
            (false, Some("$ build")),
            (true, None),
            (false, Some("$ build")),
        ] {
            let heading = visible_command_heading(&snapshot, search_open, false);
            assert_eq!(heading.map(|heading| heading.text.as_str()), expected);
        }
    }

    #[test]
    fn menu_target_follows_the_clicked_region_and_keyboard_selection() {
        let mut terminal = terminal();
        let snapshot = terminal.snapshot();
        for row in 0..3 {
            assert_eq!(block_at_row(&snapshot, row), Some(1));
        }
        for row in 3..6 {
            assert_eq!(block_at_row(&snapshot, row), Some(2));
        }
        assert_eq!(block_at_row(&snapshot, 6), None);
        assert_eq!(keyboard_block(&snapshot), Some(2));
        terminal.begin_selection(0, 1, SelectionKind::Word);
        assert_eq!(keyboard_block(&terminal.snapshot()), Some(1));
        terminal.update_selection(2, 4);
        assert_eq!(keyboard_block(&terminal.snapshot()), None);
        terminal.clear_selection();
        terminal.toggle_command_fold(1);
        assert_eq!(block_at_row(&terminal.snapshot(), 1), Some(2));
        terminal.select_command_block(1);
        assert_eq!(keyboard_block(&terminal.snapshot()), Some(1));
    }

    #[test]
    fn guide_clipping_keeps_continuations_and_only_caps_the_actual_end() {
        let mut terminal = terminal();
        let mut block = terminal.snapshot().command_blocks[0].clone();
        assert_eq!(guide_extent(&block, 20.), Some((16., 50.)));
        block.header = None;
        assert_eq!(guide_extent(&block, 20.), Some((0., 50.)));
        block.ends_here = false;
        assert_eq!(guide_extent(&block, 20.), Some((0., 60.)));
        terminal.toggle_command_fold(1);
        assert_eq!(
            guide_extent(&terminal.snapshot().command_blocks[0], 20.),
            None
        );
    }
}
