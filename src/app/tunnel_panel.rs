use super::{interaction::WindowState, ui};
use gpui::{Context, Render, Task, Timer, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    i18n::tr,
    session::{ForwardController, ForwardState, ForwardStatus},
};
use std::time::Duration;

pub struct TunnelPanel {
    controller: ForwardController,
    state: WindowState,
    statuses: Vec<ForwardStatus>,
    error: Option<String>,
    _poll: Task<()>,
}
impl TunnelPanel {
    pub fn new(controller: ForwardController, state: WindowState, cx: &mut Context<Self>) -> Self {
        let statuses = controller.statuses();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(500)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.state.is_locked() {
                            return;
                        }
                        let statuses = this.controller.statuses();
                        if this.statuses != statuses {
                            this.statuses = statuses;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            controller,
            state,
            statuses,
            error: None,
            _poll: poll,
        }
    }
}
impl Render for TunnelPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_color(rgb(ui::colors().muted))
                .child(tr(i18n_key::CONNECTIONS_PORT_FORWARDING_RULES_ARE_SAVED_IN_THE_SSH_CONNECTION_PROFILE))
                .child(tr(i18n_key::CONNECTIONS_PORT_FORWARDING_USES_A_SEPARATE_SSH_CONNECTION_AND_MAY_REQUIRE)),
        );
        if self.statuses.is_empty() {
            body = body.child(tr(
                i18n_key::CONNECTIONS_NO_PORT_FORWARDING_RULES_EDIT_THIS_SSH_CONNECTION_AND_OPEN,
            ));
        }
        for (index, status) in self.statuses.iter().enumerate() {
            let active = matches!(status.state, ForwardState::Starting | ForwardState::Running);
            let failed = matches!(status.state, ForwardState::Failed(_));
            let label = match &status.state {
                ForwardState::Starting => tr(i18n_key::CONNECTIONS_STARTING).into(),
                ForwardState::Running => tr(i18n_key::CONNECTIONS_LISTENING).into(),
                ForwardState::Stopping => tr(i18n_key::CONNECTIONS_STOPPING).into(),
                ForwardState::Stopped => tr(i18n_key::COMMON_STOPPED).into(),
                ForwardState::Failed(error) => error.clone(),
            };
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .rounded_sm()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_family(ui::MONOSPACE_FONT)
                                    .text_size(px(12.))
                                    .child(status.rule.description()),
                            )
                            .child(
                                div()
                                    .text_color(rgb(if failed {
                                        ui::colors().danger
                                    } else {
                                        ui::colors().muted
                                    }))
                                    .child(label),
                            ),
                    )
                    .child(
                        ui::text_button(
                            ("forward-toggle", index),
                            if active {
                                i18n_key::CONNECTIONS_STOP
                            } else if failed {
                                i18n_key::COMMON_RETRY
                            } else {
                                i18n_key::CONNECTIONS_START
                            },
                            ui::ButtonStyle::Neutral,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.state.is_locked() {
                                return;
                            }
                            let result = if failed {
                                this.controller.retry(index)
                            } else {
                                this.controller.set_enabled(index, !active)
                            };
                            this.error = result.err().map(|e| e.to_string());
                            cx.notify();
                        })),
                    ),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_color(rgb(ui::colors().danger))
                    .child(error.clone()),
            );
        }
        body
    }
}
