use super::{SettingsForm, SettingsSection, setting_row};
use crate::app::ui::{self, ButtonStyle, Icon};
use gpui::{Context, Div, FocusHandle, KeyDownEvent, Window, div, prelude::*, px, rgb};
use rustterm::config::{ShortcutAction, ShortcutChord};
use rustterm::i18n::{messages as key, tr};

#[derive(Default)]
enum Edit {
    #[default]
    Idle,
    Recording(ShortcutAction),
    Conflict {
        action: ShortcutAction,
        chord: ShortcutChord,
        owner: ShortcutAction,
    },
}

pub(super) struct ShortcutEditor {
    edit: Edit,
    buttons: [FocusHandle; ShortcutAction::ALL.len()],
    notice: Option<String>,
}

impl ShortcutEditor {
    pub(super) fn new(cx: &mut Context<SettingsForm>) -> Self {
        Self {
            edit: Edit::Idle,
            buttons: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            notice: None,
        }
    }
    pub(super) fn cancel(&mut self) {
        self.edit = Edit::Idle;
        self.notice = None;
    }
}

impl SettingsForm {
    fn choose_shortcut(
        &mut self,
        action: ShortcutAction,
        chord: ShortcutChord,
        cx: &mut Context<Self>,
    ) {
        self.shortcut_editor.cancel();
        if let Some(label) = chord.reserved_action() {
            self.shortcut_editor.notice = Some(rustterm::trf!(
                key::SETTINGS_SHORTCUT_FIXED,
                shortcut = chord.label(),
                action = tr(label)
            ));
            self.shortcut_editor.edit = Edit::Recording(action);
        } else if let Some(owner) = self.values.shortcuts.owner(&chord)
            && owner != action
        {
            self.shortcut_editor.edit = Edit::Conflict {
                action,
                owner,
                chord,
            };
        } else {
            self.values.shortcuts.assign(action, Some(chord));
        }
        cx.notify();
    }

    pub fn record_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.section != SettingsSection::Shortcuts
            || matches!(self.shortcut_editor.edit, Edit::Idle)
        {
            return false;
        }
        let stroke = &event.keystroke;
        if stroke.key == "escape" && !stroke.modifiers.modified() {
            self.shortcut_editor.cancel();
            cx.notify();
            return true;
        }
        let Edit::Recording(action) = self.shortcut_editor.edit else {
            return false;
        };
        if !self.shortcut_editor.buttons[action as usize].is_focused(window) {
            self.shortcut_editor.cancel();
            cx.notify();
            return false;
        }
        if event.is_held {
            return true;
        }
        if let Some(chord) = ShortcutChord::capture(stroke) {
            self.choose_shortcut(action, chord, cx);
        } else {
            self.shortcut_editor.notice = Some(tr(key::SETTINGS_SHORTCUT_UNSUPPORTED).to_owned());
            cx.notify();
        }
        true
    }

    pub(super) fn shortcuts_page(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut page = div().flex().flex_col().gap_2().child(
            div()
                .pb_2()
                .text_color(rgb(ui::colors().muted))
                .child(tr(key::SETTINGS_SHORTCUT_HINT)),
        );
        for action in ShortcutAction::ALL {
            let handle = &self.shortcut_editor.buttons[action as usize];
            let recording = matches!(self.shortcut_editor.edit, Edit::Recording(a) if a == action)
                && handle.is_focused(window);
            page = page.child(setting_row(
                action.label(),
                None,
                self.shortcut_controls(action, recording, cx),
            ));
            if let Edit::Conflict {
                action: target,
                ref chord,
                owner,
            } = self.shortcut_editor.edit
                && target == action
            {
                page = page.child(self.shortcut_conflict(chord, owner, cx));
            }
            if recording && let Some(msg) = &self.shortcut_editor.notice {
                page = page.child(
                    div()
                        .text_color(rgb(ui::colors().warning))
                        .child(msg.clone()),
                );
            }
        }
        page
    }

    fn shortcut_controls(
        &self,
        action: ShortcutAction,
        recording: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let binding = self.values.shortcuts.binding(action);
        let handle = &self.shortcut_editor.buttons[action as usize];
        let label = if recording {
            tr(key::SETTINGS_SHORTCUT_RECORD)
        } else {
            binding.map_or(tr(key::SETTINGS_SHORTCUT_UNASSIGNED), ShortcutChord::label)
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                ui::text_button_control(
                    ("shortcut-record", action as usize),
                    label.to_owned(),
                    ButtonStyle::Neutral,
                    true,
                )
                .track_focus(handle)
                .min_w(px(112.))
                .when(recording, |b| b.border_color(rgb(ui::colors().accent)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.shortcut_editor.cancel();
                    this.shortcut_editor.edit = Edit::Recording(action);
                    this.shortcut_editor.buttons[action as usize].focus(window);
                    cx.notify();
                })),
            )
            .child(
                ui::icon_button_enabled(
                    ("shortcut-clear", action as usize),
                    Icon::Close,
                    key::SETTINGS_SHORTCUT_CLEAR,
                    binding.is_some(),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.shortcut_editor.cancel();
                    this.values.shortcuts.assign(action, None);
                    cx.notify();
                })),
            )
            .child(
                ui::icon_button_enabled(
                    ("shortcut-default", action as usize),
                    Icon::Refresh,
                    key::SETTINGS_SHORTCUT_DEFAULT,
                    binding != Some(&action.default_binding()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose_shortcut(action, action.default_binding(), cx)
                })),
            )
    }

    fn shortcut_conflict(
        &self,
        chord: &ShortcutChord,
        owner: ShortcutAction,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(rgb(ui::colors().warning))
                    .child(rustterm::trf!(
                        key::SETTINGS_SHORTCUT_CONFLICT,
                        shortcut = chord.label(),
                        action = tr(owner.label())
                    )),
            )
            .child(
                ui::text_button(
                    "shortcut-replace",
                    key::SETTINGS_SHORTCUT_REPLACE,
                    ButtonStyle::Primary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Edit::Conflict { action, chord, .. } =
                        std::mem::take(&mut this.shortcut_editor.edit)
                    {
                        this.values.shortcuts.assign(action, Some(chord));
                    }
                    cx.notify();
                })),
            )
            .child(
                ui::text_button("shortcut-cancel", key::COMMON_CANCEL, ButtonStyle::Neutral)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shortcut_editor.cancel();
                        cx.notify()
                    })),
            )
    }
}
