use super::Workspace;
use crate::app::{
    pane::Pane,
    ui::{self, Icon},
};
use gpui::{AnyElement, Context, IntoElement, MouseButton, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::{config::Uuid, terminal::ShellCommand};

pub(super) struct CommandNotice {
    pub pane: usize,
    pub run: Uuid,
    pub command: ShellCommand,
}

impl Workspace {
    pub(super) fn render_command_notice(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.modal.is_some()
            || self.palette.open
            || self.menu.is_some()
            || self.utility_overlay
            || self.window_state.prompts.is_busy()
        {
            return None;
        }
        let notice = self.command_notice.as_ref()?;
        let profile = self.pane_profiles.get(&notice.pane)?;
        let label = rustterm::trf!(
            i18n_key::TERMINAL_COMMAND_FINISHED_NAME_SECONDS_S_EXIT_STATUS,
            name = profile.name,
            seconds = notice.command.duration.as_secs(),
            status = notice
                .command
                .exit_status
                .map_or_else(|| "—".into(), |code| code.to_string())
        );
        let command: String = notice
            .command
            .command
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(240)
            .collect();
        Some(
            div()
                .absolute()
                .right(px(12.))
                .bottom(px(ui::STATUS_HEIGHT + 12.))
                .w(px((f32::from(
                    crate::app::window_frame::WindowFrame::new(window)
                        .content_size(window.viewport_size())
                        .width,
                ) - 24.)
                    .clamp(1., 420.)))
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .bg(rgb(ui::colors().raised))
                .border_1()
                .border_color(rgb(ui::colors().border))
                .rounded(px(6.))
                .shadow_md()
                .occlude()
                .capture_any_mouse_down(cx.listener(|this, _, _, cx| this.release_rdp_control(cx)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(if notice.command.exit_status == Some(0) {
                                    ui::colors().success
                                } else {
                                    ui::colors().warning
                                }))
                                .child(label),
                        )
                        .child(
                            ui::icon_button(
                                "dismiss-command-notice",
                                Icon::Close,
                                i18n_key::TERMINAL_DISMISS_NOTIFICATION,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.command_notice = None;
                                    this.restore_focus(window, cx);
                                    cx.notify();
                                },
                            )),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(ui::MONOSPACE_FONT)
                        .child(command),
                )
                .child(
                    ui::text_button(
                        "view-completed-command",
                        i18n_key::TERMINAL_VIEW_COMMAND_OUTPUT,
                        ui::ButtonStyle::Neutral,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let Some(notice) = this.command_notice.take() else {
                            return;
                        };
                        if !this.lock_screen.locked {
                            this.left_tool = None;
                            this.tools.clear();
                            this.focus_pane(notice.pane, window, cx);
                            if let Some(Pane::Terminal(terminal)) = this.panes.get(&notice.pane) {
                                terminal.update(cx, |terminal, cx| {
                                    terminal.reveal_completed_command(
                                        notice.run,
                                        notice.command.id,
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }
                        cx.notify();
                    })),
                )
                .into_any_element(),
        )
    }
}
