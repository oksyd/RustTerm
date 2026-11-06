use super::{
    interaction::WindowState,
    text_input::{InputEvent, TextInput},
    ui,
    window_frame::WindowFrame,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, MouseButton, Render,
    Subscription, Task, Window, div, prelude::*, px, rgb, rgba,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{config::Secret, i18n::tr, session::AuthenticationChallenge};
use tokio::sync::oneshot;

pub struct AuthenticationChanged;
struct Pending {
    challenge: AuthenticationChallenge,
    inputs: Vec<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
    finished: oneshot::Sender<()>,
}
pub struct AuthPrompt {
    state: WindowState,
    pending: Option<Pending>,
    error: Option<&'static str>,
    focus: FocusHandle,
    _task: Task<()>,
}
impl EventEmitter<AuthenticationChanged> for AuthPrompt {}
impl AuthPrompt {
    pub fn new(state: WindowState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut receiver = state
            .take_authentication_receiver()
            .expect("one authentication view per window");
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(challenge) = receiver.recv().await {
                if challenge.reply.is_closed() || challenge.cancelled.is_cancelled() {
                    continue;
                }
                let cancelled = challenge.cancelled.clone();
                let (finished, response) = oneshot::channel();
                if this
                    .update_in(cx, |this, window, cx| {
                        let inputs: Vec<_> = challenge
                            .prompts
                            .iter()
                            .map(|prompt| {
                                cx.new(|cx| TextInput::new(prompt.label.clone(), !prompt.echo, cx))
                            })
                            .collect();
                        let subscriptions = inputs
                            .iter()
                            .enumerate()
                            .map(|(index, input)| {
                                cx.subscribe_in(input, window, move |this, _, event, window, cx| {
                                    if this.state.is_locked() {
                                        return;
                                    }
                                    match event {
                                        InputEvent::Submit => {
                                            if let Some(next) = this
                                                .pending
                                                .as_ref()
                                                .and_then(|pending| pending.inputs.get(index + 1))
                                            {
                                                next.read(cx).focus(window);
                                            } else {
                                                this.answer(window, cx);
                                            }
                                        }
                                        InputEvent::Rejected(error) => {
                                            this.error = Some(*error);
                                            cx.notify();
                                        }
                                        InputEvent::Changed => {
                                            this.error = None;
                                            cx.notify();
                                        }
                                    }
                                })
                            })
                            .collect();
                        this.error = None;
                        this.pending = Some(Pending {
                            challenge,
                            inputs,
                            _subscriptions: subscriptions,
                            finished,
                        });
                        if !this.state.is_locked() {
                            this.focus(window, cx);
                        }
                        cx.emit(AuthenticationChanged);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                tokio::select! { _=cancelled.cancelled()=>{}, _=response=>{} }
                if this
                    .update(cx, |this, cx| {
                        this.pending = None;
                        cx.emit(AuthenticationChanged);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            state,
            pending: None,
            error: None,
            focus: cx.focus_handle(),
            _task: task,
        }
    }
    pub fn is_active(&self) -> bool {
        self.pending.is_some()
    }
    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if self.state.is_locked() {
            return;
        }
        if let Some(input) = self.pending.as_ref().and_then(|p| p.inputs.first()) {
            input.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
    }
    pub fn suspend(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        if let Some(pending) = &self.pending {
            for input in &pending.inputs {
                input.update(cx, |input, cx| input.clear(cx));
            }
        }
    }
    fn answer(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.state.is_locked()
            || self
                .pending
                .as_ref()
                .is_none_or(|p| p.inputs.iter().any(|i| i.read(cx).is_composing()))
        {
            return;
        }
        if self.pending.as_ref().is_some_and(|p| {
            p.inputs
                .iter()
                .any(|input| input.read(cx).value().len() > 4096)
        }) {
            self.error =
                Some(i18n_key::CONNECTIONS_SSH_AUTHENTICATION_RESPONSES_MUST_NOT_EXCEED_4096_BYTES);
            cx.notify();
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        let answers = pending
            .inputs
            .iter()
            .map(|input| {
                let answer = Secret::new(input.read(cx).value().into());
                input.update(cx, |input, cx| input.clear(cx));
                answer
            })
            .collect();
        let _ = pending.challenge.reply.send(answers);
        let _ = pending.finished.send(());
        cx.emit(AuthenticationChanged);
        cx.notify();
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_locked() {
            self.suspend(cx);
            self.pending = None;
            cx.emit(AuthenticationChanged);
            cx.notify();
        }
    }
}
impl Render for AuthPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pending) = &self.pending else {
            return div().into_any_element();
        };
        if self.state.is_locked() {
            return div().into_any_element();
        }
        if !self.owns_focus(window, cx) {
            self.focus(window, cx);
        }
        let mut card = div()
            .id("ssh-authentication-card")
            .w(px(480.))
            .max_w_full()
            .max_h_full()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(ui::colors().surface))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded_lg()
            .child(
                div()
                    .text_size(px(17.))
                    .child(tr(i18n_key::CONNECTIONS_SSH_AUTHENTICATION)),
            )
            .child(format!(
                "{}@{}",
                pending.challenge.username, pending.challenge.host
            ));
        if !pending.challenge.name.is_empty() {
            card = card.child(pending.challenge.name.clone());
        }
        if !pending.challenge.instructions.is_empty() {
            card = card.child(
                div()
                    .text_color(rgb(ui::colors().muted))
                    .child(pending.challenge.instructions.clone()),
            );
        }
        for (prompt, input) in pending.challenge.prompts.iter().zip(&pending.inputs) {
            card = card.child(div().child(prompt.label.clone()).child(input.clone()));
        }
        if let Some(error) = self.error {
            card = card.child(div().text_color(rgb(ui::colors().danger)).child(tr(error)));
        }
        card = card.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    ui::text_button(
                        "cancel-ssh-auth",
                        i18n_key::COMMON_CANCEL,
                        ui::ButtonStyle::Neutral,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
                .child(
                    ui::text_button(
                        "submit-ssh-auth",
                        i18n_key::COMMON_CONTINUE,
                        ui::ButtonStyle::Primary,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, window, cx| this.answer(window, cx))),
                ),
        );
        div()
            .track_focus(&self.focus)
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .bg(rgba(0x101114cc))
            .map(|backdrop| WindowFrame::new(window).rounded(backdrop))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let composing = this
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.inputs.iter().any(|i| i.read(cx).is_composing()));
                if composing {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => this.cancel(cx),
                    "tab" => {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        if !this.owns_focus(window, cx) {
                            this.focus(window, cx);
                        }
                    }
                    _ => return,
                }
                window.prevent_default();
                cx.stop_propagation();
            }))
            .child(ui::scrollable(card, None))
            .into_any_element()
    }
}
