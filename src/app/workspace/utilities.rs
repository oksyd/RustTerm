use super::Workspace;
use crate::app::{
    command_view::{CommandHistoryEvent, CommandHistoryView},
    pane::{Pane, SessionTool},
    ui::{self, Icon},
};
use gpui::{AnyElement, Context, Entity, IntoElement, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

impl Workspace {
    pub(super) fn utility_is_open(&self) -> bool {
        self.tools.contains_key(&self.focused)
    }

    pub(super) fn active_commands(&self) -> Option<&Entity<CommandHistoryView>> {
        (self.tools.get(&self.focused) == Some(&SessionTool::Commands))
            .then(|| self.command_views.get(&self.focused))
            .flatten()
    }

    pub(super) fn sync_tools(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let leaves = tab.layout.leaves();
            let split = leaves.len() > 1;
            for id in leaves {
                if let Some(pane) = self.panes.get(&id) {
                    pane.set_pane_close_visible(split, cx);
                }
            }
        }
        for (id, pane) in &self.panes {
            if let Pane::Terminal(terminal) = pane {
                let tool = (*id == self.focused)
                    .then(|| self.tools.get(id).copied())
                    .flatten();
                terminal.update(cx, |terminal, cx| terminal.set_active_tool(tool, cx));
            }
        }
        for (id, files) in &self.files {
            files.update(cx, |files, cx| {
                files.set_follow_directory(self.files_follow.contains(id), cx);
            });
        }
    }

    pub(super) fn toggle_tool(
        &mut self,
        tool: SessionTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || self.input_composing(window, cx)
        {
            return;
        }
        self.menu = None;
        if self.tools.get(&self.focused) == Some(&tool) {
            self.close_utility_panel(window, cx);
            return;
        }
        match tool {
            SessionTool::Files => self.open_files(window, cx),
            SessionTool::Resources => self.open_resources(window, cx),
            SessionTool::Commands => self.open_commands(window, cx),
        }
    }

    fn open_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.focused;
        let Some(Pane::Terminal(terminal)) = self.panes.get(&pane) else {
            return;
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = self.command_views.entry(pane) {
            let terminal = terminal.clone();
            let view = cx.new(|cx| CommandHistoryView::new(terminal, window, cx));
            let subscription = cx.subscribe_in(&view, window, move |this, _, event, window, cx| {
                if this.focused != pane
                    || this.lock_screen.locked
                    || this.modal.is_some()
                    || this.window_state.prompts.is_busy()
                {
                    return;
                }
                match event {
                    CommandHistoryEvent::Close => this.close_utility_panel(window, cx),
                    CommandHistoryEvent::Reveal { run, command }
                    | CommandHistoryEvent::Insert { run, command } => {
                        if this.utility_overlay {
                            this.close_utility_panel(window, cx);
                        }
                        this.focus_pane(pane, window, cx);
                        if let Some(Pane::Terminal(terminal)) = this.panes.get(&pane) {
                            terminal.update(cx, |terminal, cx| match event {
                                CommandHistoryEvent::Reveal { .. } => {
                                    terminal.reveal_completed_command(*run, *command, window, cx);
                                }
                                CommandHistoryEvent::Insert { .. } => {
                                    terminal.insert_history_command(*run, *command, cx);
                                }
                                CommandHistoryEvent::Close => unreachable!(),
                            });
                        }
                    }
                }
            });
            entry.insert(view);
            self.command_subscriptions.insert(pane, subscription);
        }
        self.tools.insert(pane, SessionTool::Commands);
        self.command_views[&pane].read(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn close_utility_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || self.input_composing(window, cx)
        {
            return;
        }
        self.tools.remove(&self.focused);
        self.utility_overlay = false;
        self.sync_resources(window, cx);
        self.sync_visibility(cx);
        self.restore_focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_utility_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tool = *self.tools.get(&self.focused)?;
        let name = self.pane_profiles.get(&self.focused)?.name.clone();
        let (icon, label) = match tool {
            SessionTool::Files => (Icon::Folder, "SFTP"),
            SessionTool::Commands => (Icon::Terminal, i18n_key::WORKSPACE_COMMANDS),
            SessionTool::Resources => (Icon::Pulse, i18n_key::RESOURCES_RESOURCES),
        };
        let content = match tool {
            SessionTool::Files => self
                .active_files()
                .cloned()
                .map(IntoElement::into_any_element),
            SessionTool::Commands => self
                .active_commands()
                .cloned()
                .map(IntoElement::into_any_element),
            SessionTool::Resources => self.active_resources().map(IntoElement::into_any_element),
        }
        .unwrap_or_else(|| {
            div()
                .p_4()
                .text_color(rgb(ui::MUTED))
                .child(tr(i18n_key::WORKSPACE_WAITING_FOR_SSH_CONNECTION))
                .into_any_element()
        });
        Some(
            div()
                .id("session-utility")
                .track_focus(&self.utility_focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && !this.input_composing(window, cx) {
                        this.close_utility_panel(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .size_full()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .bg(rgb(ui::SURFACE))
                .child(
                    div()
                        .h(px(40.))
                        .px_3()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(rgb(ui::BORDER))
                        .child(ui::icon(icon))
                        .child(div().min_w_0().flex_1().truncate().child(name))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(ui::MUTED))
                                .child(tr(label)),
                        )
                        .child(
                            ui::icon_button(
                                "utility-hide",
                                Icon::Close,
                                i18n_key::WORKSPACE_HIDE_TOOLS,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.close_utility_panel(window, cx);
                                },
                            )),
                        ),
                )
                .child(div().flex_1().min_h_0().child(content))
                .into_any_element(),
        )
    }

    pub(super) fn render_transfer_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mut entries: Vec<_> = self
            .files
            .iter()
            .filter(|(id, files)| {
                (files.read(cx).has_active_transfers() || files.read(cx).has_failed_transfers())
                    && !(**id == self.focused && self.tools.get(id) == Some(&SessionTool::Files))
            })
            .collect();
        entries.sort_by_key(|(id, _)| **id);
        if entries.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .min_w_0()
                .overflow_hidden()
                .children(entries.into_iter().map(|(id, files)| {
                    let id = *id;
                    let files = files.read(cx);
                    let active = files.active_transfer_count();
                    let count = if active == 0 {
                        files.transfer_count()
                    } else {
                        active
                    };
                    let label = rustterm::trf!(
                        i18n_key::SFTP_SESSION_TRANSFER_COUNT,
                        session = self
                            .pane_profiles
                            .get(&id)
                            .map(|p| p.name.as_str())
                            .unwrap_or("SSH"),
                        count = count
                    );
                    div()
                        .id(("background-transfers", id))
                        .h_full()
                        .max_w(px(260.))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.))
                        .text_color(rgb(if files.has_failed_transfers() {
                            ui::WARNING
                        } else {
                            ui::MUTED
                        }))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(ui::HOVER)))
                        .tab_index(0)
                        .child(ui::icon(Icon::Folder).size(px(12.)))
                        .child(div().truncate().child(label))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.lock_screen.locked
                                || this.modal.is_some()
                                || this.window_state.prompts.is_busy()
                            {
                                return;
                            }
                            this.focus_pane(id, window, cx);
                            this.open_files(window, cx);
                        }))
                }))
                .into_any_element(),
        )
    }
}
