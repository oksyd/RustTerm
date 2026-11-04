use super::*;
use crate::app::ui::{ButtonStyle, text_button_enabled};
use gpui::{AnyElement, div, prelude::*, px, rgb, uniform_list};
use rustterm::i18n::messages as i18n_key;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Section {
    Keys,
    Server,
    SlowLog,
    PubSub,
}

impl Section {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Keys => i18n_key::REDIS_KEYS,
            Self::Server => i18n_key::REDIS_SERVER_STATUS,
            Self::SlowLog => i18n_key::REDIS_SLOW_QUERIES,
            Self::PubSub => i18n_key::REDIS_PUB_SUB,
        }
    }
}

pub(super) struct Operations {
    pub section: Section,
    rows: Vec<Message>,
    messages: std::collections::VecDeque<String>,
    message_bytes: usize,
    pub pattern: Entity<TextInput>,
    pub channel: Entity<TextInput>,
    pub payload: Entity<TextInput>,
    scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
    subscription: Option<Task<()>>,
    subscribed: bool,
    subscribing: bool,
    busy: bool,
}
impl Operations {
    pub fn new(cx: &mut Context<RedisView>) -> Self {
        Self {
            section: Section::Keys,
            rows: Vec::new(),
            messages: std::collections::VecDeque::new(),
            message_bytes: 0,
            pattern: input(i18n_key::REDIS_CHANNEL_PATTERN, "*", cx),
            channel: input(i18n_key::REDIS_PUBLISH_CHANNEL, "", cx),
            payload: input(i18n_key::REDIS_MESSAGE, "", cx),
            scroll: UniformListScrollHandle::default(),
            task: None,
            subscription: None,
            subscribed: false,
            subscribing: false,
            busy: false,
        }
    }
    pub fn disconnect(&mut self) {
        self.task = None;
        self.subscription = None;
        self.subscribed = false;
        self.subscribing = false;
        self.busy = false;
    }
}
impl RedisView {
    pub(super) fn disabled_reason(&self) -> Option<&'static str> {
        if self.busy.is_some() || self.ops.busy {
            Some(i18n_key::COMMON_WAIT_FOR_THE_CURRENT_REDIS_REQUEST_TO_FINISH)
        } else if self.native_pending {
            Some(i18n_key::COMMON_CLOSE_THE_CURRENT_DIALOG_TO_CONTINUE)
        } else if self.editor.is_some() {
            Some(i18n_key::REDIS_SAVE_OR_CANCEL_THE_CURRENT_EDIT_BEFORE_NAVIGATING)
        } else if self.state != ConnectionState::Connected {
            Some(i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION)
        } else {
            None
        }
    }

    pub(super) fn can_select_section(&self) -> bool {
        self.enabled() && !self.ops.busy && !self.window_state.is_locked()
    }

    pub(super) fn select_section(
        &mut self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.is_locked() || self.ops.busy || !self.available(cx) {
            return;
        }
        self.ops.section = section;
        if section == Section::PubSub {
            self.ops.pattern.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        self.ops.rows.clear();
        if matches!(section, Section::Server | Section::SlowLog) {
            self.refresh_operations(cx);
        }
        cx.notify();
    }

    pub(super) fn refresh_operations(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.ops.busy {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let section = self.ops.section;
        self.ops.busy = true;
        let generation = self.generation;
        let task = cx.background_executor().spawn(async move {
            if section == Section::SlowLog {
                client.slow_log().await
            } else {
                client
                    .command(0, vec![b"INFO".to_vec(), b"all".to_vec()])
                    .await
            }
        });
        self.ops.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.closed {
                    return;
                }
                this.ops.busy = false;
                match result {
                    Ok(value) if this.ops.section == section => {
                        this.ops.rows = operation_rows(section, value);
                        this.error = None;
                    }
                    Ok(_) => {}
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn toggle_subscription(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.ops.subscribing {
            return;
        }
        if self.ops.subscribed {
            self.ops.subscription = None;
            self.ops.subscribed = false;
            cx.notify();
            return;
        }
        self.ops.subscription = None;
        let Some(client) = self.client.clone() else {
            return;
        };
        let pattern = self.ops.pattern.read(cx).value().to_owned();
        let generation = self.generation;
        self.ops.subscribing = true;
        self.ops.subscription = Some(cx.spawn(async move |this, cx| {
            let mut subscription = match client.subscribe(pattern).await {
                Ok(subscription) => subscription,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.generation == generation {
                            this.ops.subscribed = false;
                            this.ops.subscribing = false;
                            this.fail(error, cx);
                        }
                    });
                    return;
                }
            };
            let ready = this
                .update(cx, |this, cx| {
                    if this.generation != generation || this.closed {
                        return false;
                    }
                    this.ops.subscribing = false;
                    this.ops.subscribed = true;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !ready {
                return;
            }
            while let Some(message) = subscription.receive().await {
                let failed = message.is_err();
                let keep = this
                    .update(cx, |this, cx| {
                        if this.generation != generation || this.closed {
                            return false;
                        }
                        match message {
                            Ok(message) => {
                                let row = format!(
                                    "{}  {}",
                                    rustterm::redis::display_bytes(&message.channel),
                                    message_preview(&message.payload)
                                );
                                this.ops.message_bytes += row.len();
                                this.ops.messages.push_back(row);
                                while this.ops.messages.len() > 500
                                    || this.ops.message_bytes > 128 * 1024
                                {
                                    if let Some(row) = this.ops.messages.pop_front() {
                                        this.ops.message_bytes -= row.len();
                                    }
                                }
                                if this.visible && !this.window_state.is_locked() {
                                    cx.notify();
                                }
                            }
                            Err(error) => {
                                this.ops.subscribed = false;
                                this.error = Some(ViewError::Backend(error));
                                cx.notify();
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep || failed {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.ops.subscribed = false;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn publish_message(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked()
            || self.ops.busy
            || self.ops.payload.read(cx).is_composing()
        {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let channel = self.ops.channel.read(cx).value().as_bytes().to_vec();
        let payload = self.ops.payload.read(cx).value().as_bytes().to_vec();
        self.ops.busy = true;
        let generation = self.generation;
        self.ops.task = Some(cx.spawn(async move |this, cx| {
            let result = client.publish(channel, payload).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.closed {
                    return;
                }
                this.ops.busy = false;
                match result {
                    Ok(RedisValue::Integer(count)) if count >= 0 => this.show_success(
                        Message::with_arg(
                            i18n_key::REDIS_MESSAGE_PUBLISHED_SUBSCRIBER_COUNT,
                            "count",
                            count,
                        ),
                        cx,
                    ),
                    Ok(_) => this.fail(RedisError::Protocol, cx),
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_operations(&self, cx: &mut Context<Self>) -> AnyElement {
        let pubsub = self.ops.section == Section::PubSub;
        let mut toolbar = div().flex().items_center().gap_2().p_2().flex_shrink_0();
        if pubsub {
            toolbar = toolbar
                .child(div().flex_1().child(self.ops.pattern.clone()))
                .child(
                    text_button_enabled(
                        "redis-subscribe",
                        if self.ops.subscribing {
                            i18n_key::COMMON_CONNECTING_PROGRESS
                        } else if self.ops.subscribed {
                            i18n_key::REDIS_STOP_SUBSCRIPTION
                        } else {
                            i18n_key::REDIS_SUBSCRIBE
                        },
                        ButtonStyle::Neutral,
                        !self.ops.subscribing && (self.ops.subscribed || self.enabled()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_subscription(cx))),
                )
                .child(
                    text_button_enabled(
                        "clear-messages",
                        i18n_key::REDIS_CLEAR,
                        ButtonStyle::Neutral,
                        !self.ops.messages.is_empty(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.window_state.is_locked() {
                            return;
                        }
                        this.ops.messages.clear();
                        this.ops.message_bytes = 0;
                        cx.notify();
                    })),
                );
        } else {
            toolbar = toolbar
                .child(div().flex_1().text_color(rgb(ui::MUTED)).child(tr(
                    if self.ops.section == Section::Server {
                        i18n_key::REDIS_STATUS_OF_THE_CONNECTED_REDIS_NODE
                    } else {
                        i18n_key::REDIS_MOST_RECENT_100_SLOW_QUERIES_DURATION_IN_MICROSECONDS
                    },
                )))
                .child(
                    text_button_enabled(
                        "refresh-redis-status",
                        if self.ops.busy {
                            i18n_key::COMMON_LOADING
                        } else {
                            i18n_key::COMMON_REFRESH
                        },
                        ButtonStyle::Neutral,
                        self.can_select_section(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_operations(cx))),
                );
        }
        let count = if pubsub {
            self.ops.messages.len()
        } else {
            self.ops.rows.len()
        };
        let list = uniform_list(
            "redis-operations-list",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, _| {
                range
                    .map(|index| {
                        div()
                            .h(px(25.))
                            .px_3()
                            .text_size(px(12.))
                            .font_family(ui::MONOSPACE_FONT)
                            .overflow_hidden()
                            .child(
                                if pubsub {
                                    this.ops.messages.get(index).cloned()
                                } else {
                                    this.ops.rows.get(index).map(Message::render)
                                }
                                .unwrap_or_default(),
                            )
                    })
                    .collect()
            }),
        )
        .map(ui::preserve_scroll_axes)
        .track_scroll(self.ops.scroll.clone())
        .flex_1()
        .min_h_0();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(list)
                    .child(ui::scrollbars(
                        "redis-operations-bars",
                        None,
                        Some(self.ops.scroll.0.borrow().base_handle.clone()),
                    )),
            )
            .when(pubsub, |body| {
                body.child(
                    div()
                        .flex()
                        .gap_2()
                        .p_2()
                        .child(div().w(px(150.)).child(self.ops.channel.clone()))
                        .child(div().flex_1().child(self.ops.payload.clone()))
                        .child(
                            text_button_enabled(
                                "publish-message",
                                i18n_key::REDIS_PUBLISH,
                                ButtonStyle::Primary,
                                self.can_select_section(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.publish_message(cx))),
                        ),
                )
            })
            .into_any_element()
    }
}

fn operation_rows(section: Section, value: RedisValue) -> Vec<Message> {
    if section == Section::Server
        && let RedisValue::Bytes(bytes) = value
    {
        let text = String::from_utf8_lossy(&bytes);
        return text
            .lines()
            .filter(|line| !line.is_empty())
            .take(2048)
            .map(|line| line.replace(':', "    ").into())
            .collect();
    }
    let RedisValue::Array(entries) = value else {
        return vec![i18n_key::REDIS_NO_RESULTS.into()];
    };
    entries
        .into_iter()
        .take(100)
        .map(|entry| {
            let RedisValue::Array(fields) = entry else {
                return i18n_key::REDIS_INVALID_RESPONSE.into();
            };
            if fields.len() < 4 {
                return i18n_key::REDIS_INVALID_RESPONSE.into();
            }
            let command = match &fields[3] {
                RedisValue::Array(args) => {
                    args.iter().map(format_reply).collect::<Vec<_>>().join(" ")
                }
                value => format_reply(value),
            };
            format!(
                "#{}  {}  {} µs  {}",
                format_reply(&fields[0]),
                format_reply(&fields[1]),
                format_reply(&fields[2]),
                command
            )
            .into()
        })
        .collect()
}

fn message_preview(bytes: &[u8]) -> String {
    let mut end = bytes.len().min(4096);
    if let Ok(text) = std::str::from_utf8(bytes) {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    let mut preview = rustterm::redis::display_bytes(&bytes[..end]);
    if end < bytes.len() {
        preview.push('…');
    }
    preview
}
