use super::{Modal, Workspace};
use crate::app::Startup;
use crate::app::forms::SettingsSection;
use crate::app::layout::Axis;
use crate::app::pane::Pane;
use crate::app::text_input::{InputEvent, TextInput};
use crate::app::ui;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, KeyDownEvent, MouseDownEvent, MouseMoveEvent, Subscription,
    Window, canvas, div, px, rgb,
};
use rustterm::config::Secret;
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};
use std::time::Duration;

use crate::app::forms::LockChange;
use gpui::{DispatchPhase, ScrollWheelEvent, Task, Timer};
use rustterm::{app_lock, config::AppLockConfig};
use std::time::Instant;

pub(super) struct LockState {
    pub(super) locked: bool,
    password: Entity<TextInput>,
    unlock_hash: Option<String>,
    pending_initial: Option<Startup>,
    busy: bool,
    pub(super) error: Option<Message>,
    failures: u32,
    retry_at: Instant,
    last_activity: Instant,
    _subscription: Subscription,
    idle_task: Option<Task<()>>,
    retry_task: Option<Task<()>>,
    operation: Option<Task<()>>,
}

impl LockState {
    pub(super) fn new(
        config: &AppLockConfig,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let password = cx.new(|cx| {
            let mut input = TextInput::new("", true, cx);
            input.set_large(true, cx);
            input.set_password_visibility_toggle(true, cx);
            input
        });
        let subscription =
            cx.subscribe_in(
                &password,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Submit => this.unlock_workspace(window, cx),
                    InputEvent::Changed => {
                        this.lock_screen.error = None;
                        this.lock_screen
                            .password
                            .update(cx, |input, cx| input.set_invalid(false, cx));
                        cx.notify();
                    }
                    InputEvent::Rejected(message) => {
                        this.lock_screen.error = Some((*message).into());
                        cx.notify();
                    }
                },
            );
        Self {
            locked: config.password_hash.is_some(),
            unlock_hash: config.password_hash.clone(),
            password,
            pending_initial: None,
            busy: false,
            error: None,
            failures: 0,
            retry_at: Instant::now(),
            last_activity: Instant::now(),
            _subscription: subscription,
            idle_task: None,
            retry_task: None,
            operation: None,
        }
    }
}

impl Workspace {
    pub(super) fn can_change_lock_settings(&mut self, cx: &mut Context<Self>) -> bool {
        if self.can_verify_lock(cx) {
            true
        } else {
            self.error = self.lock_screen.error.take();
            false
        }
    }

    pub(super) fn reject_lock_change(&mut self, error: LockChangeError) {
        if matches!(error, LockChangeError::IncorrectPassword) {
            self.rejected_password();
            self.error = self.lock_screen.error.take();
        } else {
            self.error = Some(Message::from_error(error));
        }
    }

    pub(super) fn apply_lock_settings(
        &mut self,
        settings: AppLockConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.config.app_lock = settings;
        if self.lock_screen.locked && self.config.app_lock.password_hash.is_some() {
            self.lock_screen.unlock_hash = self.config.app_lock.password_hash.clone();
        }
        self.lock_screen.failures = 0;
        self.lock_screen.retry_at = Instant::now();
        self.lock_screen.retry_task = None;
        self.note_activity();
        self.schedule_idle_lock(window, cx);
    }

    pub(super) fn focus_lock_password(&self, window: &mut Window, cx: &App) {
        self.lock_screen.password.read(cx).focus(window);
    }

    fn set_lock_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.lock_screen.busy = busy;
        self.lock_screen
            .password
            .update(cx, |input, cx| input.set_read_only(busy, cx));
    }

    pub(super) fn close_locked(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.lock_screen.busy
            || self.modal_saving
            || self.pending_credentials > 0
            || self.confirming_close
        {
            return false;
        }
        if self.save_failed {
            self.lock_screen.error = Some(
                i18n_key::TERMINAL_COULD_NOT_SAVE_SETTINGS_UNLOCK_TO_REVIEW_THE_ERROR_AND.into(),
            );
            cx.notify();
            return false;
        }
        if self.saving || self.flush_pending_workspace(cx) {
            self.confirming_close = true;
            self.set_lock_busy(true, cx);
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    Timer::after(Duration::from_millis(100)).await;
                    let waiting = this.update(cx, |this, _| this.saving).unwrap_or(false);
                    if !waiting {
                        break;
                    }
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    this.confirming_close = false;
                    this.set_lock_busy(false, cx);
                    this.close_locked(window, cx);
                });
            })
            .detach();
            cx.notify();
            return false;
        }
        self.closing = true;
        self.files.clear();
        self.files_subscriptions.clear();
        for pane in self.panes.values() {
            pane.close(cx);
        }
        window.remove_window();
        true
    }
    pub(super) fn initialize_lock(
        &mut self,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lock_screen.locked {
            self.window_state.set_locked(true);
            self.lock_screen.pending_initial = Some(startup);
            window.set_window_title(&format!("RustTerm — {}", tr(i18n_key::SETTINGS_LOCKED)));
            self.lock_screen.password.read(cx).focus(window);
        } else {
            self.open_initial(startup, window, cx);
            self.schedule_idle_lock(window, cx);
        }
    }

    fn open_initial(&mut self, startup: Startup, window: &mut Window, cx: &mut Context<Self>) {
        let Startup {
            profile: initial,
            panes,
            restore,
            ..
        } = startup;
        if self.restore_saved_workspace(restore, window, cx) {
            return;
        }
        self.open_profile(initial.clone(), None, window, cx);
        if panes == 4 {
            let first = self.focused;
            for (target, axis) in [(first, Axis::Horizontal), (first, Axis::Vertical)] {
                self.open_profile_at(initial.clone(), None, Some((target, axis)), window, cx);
            }
            let second = self
                .visible_ids()
                .into_iter()
                .find(|id| *id != first && *id != self.focused);
            if let Some(target) = second {
                self.open_profile_at(initial, None, Some((target, Axis::Vertical)), window, cx);
            }
        }
    }

    pub(super) fn note_activity(&mut self) {
        if !self.lock_screen.locked {
            self.lock_screen.last_activity = Instant::now();
        }
    }

    fn schedule_idle_lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lock_screen.idle_task = None;
        if self.lock_screen.locked
            || self.config.app_lock.password_hash.is_none()
            || self.config.app_lock.idle_minutes == 0
        {
            return;
        }
        self.lock_screen.idle_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let remaining = this
                    .update(cx, |this, _| {
                        if this.lock_screen.locked || this.config.app_lock.idle_minutes == 0 {
                            None
                        } else {
                            Some(
                                Duration::from_secs(
                                    u64::from(this.config.app_lock.idle_minutes) * 60,
                                )
                                .saturating_sub(this.lock_screen.last_activity.elapsed()),
                            )
                        }
                    })
                    .ok()
                    .flatten();
                let Some(remaining) = remaining else { return };
                if remaining.is_zero() {
                    let _ = this.update_in(cx, |this, window, cx| this.lock_workspace(window, cx));
                    return;
                }
                Timer::after(remaining).await;
            }
        }));
    }

    pub(super) fn lock_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked || self.closing {
            return;
        }
        self.release_rdp_control(cx);
        let Some(hash) = self.config.app_lock.password_hash.clone() else {
            self.open_settings(SettingsSection::Security, window, cx);
            return;
        };
        self.persist_workspace(cx);
        self.lock_screen.locked = true;
        self.palette.open = false;
        self.authentication
            .update(cx, |prompt, cx| prompt.suspend(cx));
        self.lock_screen.unlock_hash = Some(hash);
        self.lock_screen.error = None;
        self.lock_screen
            .password
            .update(cx, |input, cx| input.clear(cx));
        self.window_state.set_locked(true);
        self.cancel_profile_test();
        for pane in self.panes.values() {
            pane.set_locked(true, cx);
            pane.set_visible(false, cx);
        }
        if let Some(Modal::Settings(form)) = &self.modal {
            form.update(cx, |form, cx| form.clear_lock_secrets(cx));
        }
        if let Some(Modal::Profile(form)) = &self.modal {
            form.update(cx, |form, cx| form.clear_sensitive(cx));
        }
        self.menu = None;
        self.dragging = None;
        self.sidebar_overlay_open = false;
        self.popup_return_focus = None;
        window.set_window_title(&format!("RustTerm — {}", tr(i18n_key::SETTINGS_LOCKED)));
        self.lock_screen.password.read(cx).focus(window);
        self.refresh_lock_retry(cx);
        cx.notify();
    }

    fn can_verify_lock(&mut self, cx: &mut Context<Self>) -> bool {
        if self.lock_screen.busy {
            return false;
        }
        if Instant::now() < self.lock_screen.retry_at {
            self.lock_screen.error =
                Some(i18n_key::SETTINGS_TOO_MANY_ATTEMPTS_WAIT_A_MOMENT_AND_TRY_AGAIN.into());
            cx.notify();
            return false;
        }
        true
    }

    fn rejected_password(&mut self) {
        self.lock_screen.failures = self.lock_screen.failures.saturating_add(1);
        self.lock_screen.retry_at = Instant::now() + retry_delay(self.lock_screen.failures);
        self.lock_screen.error = Some(i18n_key::SETTINGS_INCORRECT_LOCK_PASSWORD.into());
    }

    fn refresh_lock_retry(&mut self, cx: &mut Context<Self>) {
        self.lock_screen.retry_task = None;
        let deadline = self.lock_screen.retry_at;
        if deadline <= Instant::now() {
            return;
        }
        self.lock_screen.retry_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                Timer::after(remaining.min(Duration::from_secs(1))).await;
                let locked = this.update(cx, |this, cx| {
                    cx.notify();
                    this.lock_screen.locked
                });
                if !matches!(locked, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn unlock_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.lock_screen.locked
            || self.modal_saving
            || self.lock_screen.password.read(cx).value().is_empty()
            || self.lock_screen.password.read(cx).is_composing()
            || Instant::now() < self.lock_screen.retry_at
            || !self.can_verify_lock(cx)
        {
            return;
        }
        let Some(hash) = self.lock_screen.unlock_hash.clone() else {
            return;
        };
        let password = Secret::new(self.lock_screen.password.read(cx).value().to_owned());
        self.set_lock_busy(true, cx);
        self.lock_screen.error = None;
        let task = cx
            .background_executor()
            .spawn(async move { app_lock::verify(password.expose(), &hash) });
        self.lock_screen.operation = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.lock_screen
                    .password
                    .update(cx, |input, cx| input.clear(cx));
                this.set_lock_busy(false, cx);
                match result {
                    Ok(true) => {
                        this.lock_screen.locked = false;
                        this.lock_screen.unlock_hash = None;
                        this.lock_screen.failures = 0;
                        this.lock_screen.retry_at = Instant::now();
                        this.lock_screen.retry_task = None;
                        this.window_state.set_locked(false);
                        for pane in this.panes.values() {
                            pane.set_locked(false, cx);
                        }
                        if let Some(startup) = this.lock_screen.pending_initial.take() {
                            this.open_initial(startup, window, cx);
                        } else {
                            this.set_active(this.active, window, cx);
                            this.restore_focus(window, cx);
                        }
                        this.note_activity();
                        this.schedule_idle_lock(window, cx);
                    }
                    Ok(false) => {
                        this.rejected_password();
                        this.lock_screen
                            .password
                            .update(cx, |input, cx| input.set_invalid(true, cx));
                        this.refresh_lock_retry(cx);
                    }
                    Err(error) => this.lock_screen.error = Some(Message::from_error(error)),
                }
                if this.lock_screen.locked {
                    this.lock_screen.password.read(cx).focus(window);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn lock_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_activity();
        if self.lock_screen.locked {
            self.focus_lock_password(window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if !self.lock_screen.locked
            && event.keystroke.key == "l"
            && modifiers.control
            && modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
        {
            self.lock_workspace(window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    pub(super) fn render_locked(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let retry = retry_seconds(self.lock_screen.retry_at, Instant::now());
        let saving = self.modal_saving || self.confirming_close;
        let enabled = !saving
            && !self.lock_screen.busy
            && retry == 0
            && !self.lock_screen.password.read(cx).value().is_empty();
        let label = if saving {
            tr(i18n_key::WORKSPACE_SAVING_CHANGES).to_owned()
        } else if self.lock_screen.busy {
            tr(i18n_key::SETTINGS_UNLOCKING).to_owned()
        } else if retry > 0 {
            rustterm::trf!(i18n_key::SETTINGS_RETRY_IN_SECONDS_S, seconds = retry)
        } else {
            tr(i18n_key::SETTINGS_UNLOCK).to_owned()
        };
        let action = div()
            .id("unlock-workspace")
            .relative()
            .w_full()
            .h(px(42.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(ui::colors().accent))
            .bg(rgb(ui::colors().accent))
            .text_color(rgb(ui::colors().on_accent))
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .tab_index(0)
            .tab_stop(enabled)
            .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ui::colors().accent_hover)))
                    .focus(|style| style.border_color(rgb(ui::colors().text)))
                    .on_click(cx.listener(|this, _, window, cx| this.unlock_workspace(window, cx)))
            })
            .when(!enabled, |button| button.opacity(0.45))
            .child(label)
            .when(!saving && !self.lock_screen.busy && retry == 0, |button| {
                button.child(
                    ui::icon(ui::Icon::Enter)
                        .absolute()
                        .right(px(14.))
                        .size(px(20.))
                        .text_color(rgb(ui::colors().on_accent)),
                )
            });
        let form = div()
            .w(px(344.))
            .max_w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .child(
                ui::icon(ui::Icon::Logo)
                    .size(px(48.))
                    .mb(px(22.))
                    .text_color(rgb(ui::colors().brand)),
            )
            .child(
                div()
                    .h(px(32.))
                    .mb(px(28.))
                    .text_size(px(25.))
                    .line_height(px(32.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(tr(i18n_key::SETTINGS_LOCKED)),
            )
            .child(
                div()
                    .w_full()
                    .h(px(16.))
                    .mb(px(12.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::SETTINGS_LOCK_PASSWORD)),
            )
            .child(self.lock_screen.password.clone())
            .child(
                div()
                    .w_full()
                    .min_h(px(36.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(rgb(ui::colors().danger))
                    .children(self.lock_screen.error.as_ref().map(Message::render)),
            )
            .child(action);
        let frame = crate::app::window_frame::WindowFrame::new(window);
        let content_size = frame.content_size(window.viewport_size());
        let top = ((f32::from(content_size.height) - ui::TITLE_HEIGHT - 280.) / 2. - 40.).max(24.);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().bg))
            .map(|d| frame.rounded(d))
            .overflow_hidden()
            .pb(frame.corners.bottom_left.max(frame.corners.bottom_right))
            .text_color(rgb(ui::colors().text))
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .track_focus(&self.workspace_focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !window.default_prevented() {
                        this.focus_lock_password(window, cx);
                        window.prevent_default();
                    }
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !this.lock_screen.password.read(cx).is_composing()
                {
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    if !this.workspace_focus.contains_focused(window, cx) {
                        this.focus_lock_password(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                }
                // Linux commits text through EntityInputHandler only for unconsumed keys.
            }))
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .id("lock-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .px(px(24.))
                            .pt(px(top))
                            .pb(px(24.))
                            .child(form),
                    )
                    .map(|content| ui::scrollable(content, None)),
            )
            .into_any_element()
    }

    // Register in canvas paint, before child handlers, to count scroll/motion even
    // when terminal/remote-desktop views consume the event. No redraw per motion.
    pub(super) fn activity_listener(&self, cx: &Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let press = weak.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let _ = press.update(cx, |this, cx| {
                        this.note_activity();
                        let covered = this.closing
                            || this.modal_saving
                            || this.menu.is_some()
                            || this.palette.open
                            || this.modal.is_some()
                            || this.authentication.read(cx).is_active()
                            || this.sidebar_overlay_open
                            || this.utility_overlay
                            || this.window_state.prompts.is_busy();
                        for pane in this.panes.values() {
                            if let Pane::Rdp(view) = pane {
                                let release = {
                                    let view = view.read(cx);
                                    view.captures_keyboard()
                                        && (covered || !view.contains_desktop(event.position))
                                };
                                if release {
                                    view.update(cx, |view, cx| view.release_control(cx));
                                }
                            }
                        }
                    });
                });
                let motion = weak.clone();
                window.on_mouse_event(move |_: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = motion.update(cx, |this, _| this.note_activity());
                    }
                });
                let scroll = weak.clone();
                window.on_mouse_event(move |_: &ScrollWheelEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = scroll.update(cx, |this, _| this.note_activity());
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1_u64 << failures.saturating_sub(1).min(5))
}

fn retry_seconds(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)
}

#[derive(Debug, thiserror::Error)]
pub(super) enum LockChangeError {
    #[error("{}", tr(i18n_key::SETTINGS_INCORRECT_LOCK_PASSWORD))]
    IncorrectPassword,
    #[error(transparent)]
    Password(#[from] app_lock::Error),
}

pub(super) fn resolve_lock_change(
    previous: &AppLockConfig,
    change: LockChange,
) -> Result<AppLockConfig, LockChangeError> {
    if let Some(hash) = &previous.password_hash
        && !app_lock::verify(change.current.expose(), hash)?
    {
        return Err(LockChangeError::IncorrectPassword);
    }
    if !change.enabled {
        return Ok(AppLockConfig::default());
    }
    let password_hash = if change.password.expose().is_empty() && previous.password_hash.is_some() {
        previous.password_hash.clone()
    } else {
        Some(app_lock::create(change.password.expose())?)
    };
    Ok(AppLockConfig {
        password_hash,
        idle_minutes: change.idle_minutes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn countdown_stays_positive_until_retry_is_allowed() {
        let now = Instant::now();
        assert_eq!(retry_seconds(now + Duration::from_millis(1001), now), 2);
        assert_eq!(retry_seconds(now + Duration::from_secs(1), now), 1);
        assert_eq!(retry_seconds(now + Duration::from_nanos(1), now), 1);
        assert_eq!(retry_seconds(now, now), 0);
        assert_eq!(retry_seconds(now, now + Duration::from_secs(1)), 0);
    }

    #[test]
    fn repeated_password_attempts_have_a_bounded_delay() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(32));
    }

    #[test]
    fn lock_settings_require_current_password_and_keep_hash_for_timeout_changes() {
        let password = "settings test password";
        let previous = AppLockConfig {
            password_hash: Some(app_lock::create(password).unwrap()),
            idle_minutes: 10,
        };
        let change = |current: &str, enabled, idle_minutes| LockChange {
            current: Secret::new(current.to_owned()),
            password: Secret::new(String::new()),
            enabled,
            idle_minutes,
        };
        assert!(matches!(
            resolve_lock_change(&previous, change("incorrect", false, 0)),
            Err(LockChangeError::IncorrectPassword)
        ));
        let updated = resolve_lock_change(&previous, change(password, true, 20)).unwrap();
        assert_eq!(updated.password_hash, previous.password_hash);
        assert_eq!(updated.idle_minutes, 20);
        let removed = resolve_lock_change(&updated, change(password, false, 0)).unwrap();
        assert!(removed.password_hash.is_none());
        assert_eq!(removed.idle_minutes, 0);
    }
}
