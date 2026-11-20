use super::{InputEvent, SettingsForm, TextInput, column, hint, setting_row, ui};
use gpui::{
    Context, CursorStyle, Div, Entity, HighlightStyle, MouseButton, MouseDownEvent, StyledText,
    Subscription, Window, div, prelude::*, px, rgb,
};
use rustterm::i18n::messages as key;
use rustterm::terminal::{SelectionKind, Terminal, default_word_delimiters, valid_word_delimiters};

// ASCII makes each displayed byte index a terminal column.
const SAMPLE: &str = "api-prod-01  /srv/app/logs";

pub(super) struct WordSelectionPreview {
    terminal: Terminal,
    column: Option<u16>,
    _field: Subscription,
}

impl WordSelectionPreview {
    pub(super) fn new(field: Entity<TextInput>, cx: &mut Context<Self>) -> Self {
        let mut terminal = Terminal::new(SAMPLE.len() as u16, 1, 0);
        terminal.set_word_delimiters(field.read(cx).value());
        terminal.feed(SAMPLE.as_bytes());
        Self {
            terminal,
            column: None,
            _field: cx.subscribe(&field, |this, field, event, cx| {
                let punctuation = field.read(cx).value();
                if matches!(event, InputEvent::Changed) && valid_word_delimiters(punctuation) {
                    this.terminal.set_word_delimiters(punctuation);
                    this.select(this.column);
                    cx.notify();
                }
            }),
        }
    }

    fn select(&mut self, column: Option<u16>) {
        self.column = column;
        if let Some(column) = column {
            self.terminal
                .begin_selection(column, 0, SelectionKind::Word);
        } else {
            self.terminal.clear_selection();
        }
    }
}

impl Render for WordSelectionPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = self.terminal.snapshot().lines.first().and_then(|line| {
            let first = line.cells.iter().position(|cell| cell.selected)?;
            let last = line.cells.iter().rposition(|cell| cell.selected)?;
            Some(first..last + 1)
        });
        let text = StyledText::new(SAMPLE).with_highlights(selection.into_iter().map(|range| {
            (
                range,
                HighlightStyle {
                    color: Some(rgb(ui::colors().on_accent).into()),
                    background_color: Some(rgb(ui::colors().accent).into()),
                    ..Default::default()
                },
            )
        }));
        let layout = text.layout().clone();
        div()
            .id("word-selection-preview")
            .p_2()
            .rounded(px(4.))
            .bg(rgb(ui::colors().input))
            .whitespace_nowrap()
            .overflow_hidden()
            .cursor(CursorStyle::IBeam)
            .child(text)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    let column = if event.click_count == 2 {
                        layout
                            .index_for_position(event.position)
                            .ok()
                            .filter(|index| *index < SAMPLE.len())
                            .map(|index| index as u16)
                    } else {
                        None
                    };
                    this.select(column);
                    cx.notify();
                }),
            )
    }
}

impl SettingsForm {
    pub(super) fn word_selection_settings(&self, cx: &Context<Self>) -> Div {
        let field = self.fields.field(key::SETTINGS_WORD_DELIMITERS);
        let customized = field.read(cx).value() != default_word_delimiters();
        column()
            .child(setting_row(
                key::SETTINGS_WORD_DELIMITERS,
                Some(key::SETTINGS_WORD_DELIMITERS_HELP),
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().w(px(200.)).child(field.clone()))
                    .child(
                        ui::icon_button_enabled(
                            "word-delimiters-default",
                            ui::Icon::Refresh,
                            key::SETTINGS_SHORTCUT_DEFAULT,
                            customized,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.fields.field(key::SETTINGS_WORD_DELIMITERS).update(
                                cx,
                                |field, cx| {
                                    field.set_value(default_word_delimiters(), cx);
                                    cx.emit(InputEvent::Changed);
                                },
                            );
                        })),
                    ),
            ))
            .child(hint(key::SETTINGS_WORD_DELIMITERS_PREVIEW))
            .child(
                div()
                    .font_family(self.values.font_family.clone())
                    .child(self.word_preview.clone()),
            )
    }
}
