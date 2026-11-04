use super::*;
use rustterm::i18n::messages as i18n_key;

struct EditCommand {
    arguments: Vec<Vec<u8>>,
    key: Vec<u8>,
    expected: ExpectedReply,
    rejection: &'static str,
}

impl EditAction {
    /// Name, value and extra fields that hold Redis bytes (rather than numeric parameters).
    pub(super) fn text_fields(&self) -> [bool; 3] {
        [
            matches!(self, Self::Create(_) | Self::Rename),
            !matches!(self, Self::Rename | Self::Ttl | Self::ZSet(Some(_))),
            matches!(
                self,
                Self::Create(DataKind::Hash | DataKind::Stream) | Self::Hash(None) | Self::Stream
            ),
        ]
    }
}

impl RedisView {
    pub(super) fn set_editor_read_only(&self, read_only: bool, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            for input in [&editor.name, &editor.value, &editor.extra] {
                input.update(cx, |input, cx| input.set_read_only(read_only, cx));
            }
        }
    }

    pub(super) fn create_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.available(cx) {
            self.edit(
                EditAction::Create(DataKind::String),
                "",
                "",
                "field",
                window,
                cx,
            );
        }
    }

    pub(super) fn rename_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(key) = self.selected_key.as_deref() else {
            return;
        };
        let Some(name) = value::editable(key) else {
            self.error = Some(
                i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR.into(),
            );
            cx.notify();
            return;
        };
        self.edit(EditAction::Rename, &name, "", "", window, cx);
    }

    pub(super) fn edit_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(info) = &self.info else { return };
        let seconds = if info.ttl_ms >= 0 {
            (info.ttl_ms / 1000 + i64::from(info.ttl_ms % 1000 != 0)).to_string()
        } else {
            "-1".into()
        };
        self.edit(EditAction::Ttl, "", &seconds, "", window, cx);
    }

    pub(super) fn edit_value(&mut self, append: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(info) = &self.info else { return };
        let row = self.selected_row.unwrap_or(0);
        let draft: Option<(EditAction, String, String, String)> = match &info.data {
            KeyData::String(bytes) if !append => {
                if value::partial_string(info, &self.detail_page) {
                    self.error = Some(
                        i18n_key::REDIS_THIS_VALUE_IS_PAGED_ONLY_A_COMPLETE_VALUE_CAN_BE.into(),
                    );
                    cx.notify();
                    return;
                }
                value::editable(bytes).map(|text| (EditAction::String, "".into(), text, "".into()))
            }
            KeyData::Hash(_) if append => {
                Some((EditAction::Hash(None), "".into(), "".into(), "field".into()))
            }
            KeyData::Hash(rows) => rows.get(row).and_then(|(field, value)| {
                value::editable(value).map(|text| {
                    (
                        EditAction::Hash(Some(field.clone())),
                        "".into(),
                        text,
                        value::label(field),
                    )
                })
            }),
            KeyData::List(_) if append => {
                Some((EditAction::List(None), "".into(), "".into(), "".into()))
            }
            KeyData::List(rows) => rows.get(row).and_then(|value| {
                value::editable(value).map(|text| {
                    (
                        EditAction::List(Some((
                            self.detail_page.offset + row as u64,
                            value.clone(),
                        ))),
                        "".into(),
                        text,
                        "".into(),
                    )
                })
            }),
            KeyData::Set(_) if append => Some((EditAction::Set, "".into(), "".into(), "".into())),
            KeyData::ZSet(_) if append => {
                Some((EditAction::ZSet(None), "".into(), "".into(), "0".into()))
            }
            KeyData::ZSet(rows) => rows.get(row).map(|(member, score)| {
                (
                    EditAction::ZSet(Some(member.clone())),
                    "".into(),
                    value::label(member),
                    String::from_utf8_lossy(score).into_owned(),
                )
            }),
            KeyData::Stream(_) if append => {
                Some((EditAction::Stream, "".into(), "".into(), "field".into()))
            }
            _ => return,
        };
        if let Some((action, name, text, extra)) = draft {
            self.edit(action, &name, &text, &extra, window, cx);
        } else {
            self.error = Some(
                i18n_key::REDIS_BINARY_OR_OVERSIZED_VALUES_ARE_READ_ONLY_IN_THIS_EDITOR.into(),
            );
            cx.notify();
        }
    }

    fn edit(
        &mut self,
        action: EditAction,
        name: &str,
        text: &str,
        extra: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields = action.text_fields();
        let initial = [name, text, extra];
        let encoding = if initial.iter().enumerate().all(|(index, text)| {
            !fields[index]
                || value::EditEncoding::Text
                    .encode(text.as_bytes(), index == 1)
                    .is_ok()
        }) {
            value::EditEncoding::Text
        } else {
            value::EditEncoding::Escaped
        };
        let drafts: Result<Vec<_>, _> = initial
            .iter()
            .enumerate()
            .map(|(index, text)| {
                if fields[index] {
                    encoding.encode(text.as_bytes(), index == 1)
                } else {
                    Ok((*text).to_owned())
                }
            })
            .collect();
        let drafts = match drafts {
            Ok(drafts) => drafts,
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        let [name, value, extra] = [0, 1, 2].map(|index| {
            cx.new(|cx| {
                let mut input = TextInput::new(
                    [
                        i18n_key::REDIS_KEY,
                        i18n_key::CONNECTIONS_VALUE,
                        i18n_key::REDIS_FIELD_OR_SCORE,
                    ][index],
                    false,
                    cx,
                );
                input.set_compact(true, cx);
                input.set_literal(
                    index == 1 && fields[index] && encoding == value::EditEncoding::Text,
                    cx,
                );
                input.set_value(&drafts[index], cx);
                input
            })
        });
        let subscriptions = [&name, &value, &extra]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event, cx| match event {
                    InputEvent::Submit => this.save_editor(cx),
                    InputEvent::Rejected(message) => {
                        this.error = Some((*message).into());
                        cx.notify();
                    }
                    InputEvent::Changed => {}
                })
            })
            .collect();
        if matches!(action, EditAction::Create(_) | EditAction::Rename) {
            name.read(cx).focus(window);
        } else if matches!(action, EditAction::ZSet(Some(_))) {
            extra.read(cx).focus(window);
        } else {
            value.read(cx).focus(window);
        }
        self.console_open = false;
        self.editor = Some(Editor {
            action,
            encoding,
            name,
            value,
            extra,
            _subscriptions: subscriptions,
        });
        self.error = None;
        cx.notify();
    }

    pub(super) fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_none() {
            self.editor = None;
            self.error = None;
            self.focus.focus(window);
            cx.notify();
        }
    }

    pub(super) fn set_edit_encoding(
        &mut self,
        encoding: value::EditEncoding,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(editor) = &self.editor else { return };
        if editor.encoding == encoding {
            return;
        }
        let fields = editor.action.text_fields();
        let inputs = [&editor.name, &editor.value, &editor.extra];
        let result: Result<Vec<_>, _> = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                if fields[index] {
                    let bytes = editor.encoding.parse(input.read(cx).value())?;
                    encoding.encode(&bytes, index == 1).map(Some)
                } else {
                    Ok(None)
                }
            })
            .collect();
        let drafts = match result {
            Ok(drafts) => drafts,
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        for (index, (input, draft)) in inputs.into_iter().zip(drafts).enumerate() {
            if let Some(draft) = draft {
                input.update(cx, |input, cx| {
                    input.set_literal(index == 1 && encoding == value::EditEncoding::Text, cx);
                    input.set_value(draft, cx);
                });
            }
        }
        self.editor.as_mut().expect("editor was checked").encoding = encoding;
        self.error = None;
        cx.notify();
    }

    pub(super) fn set_edit_kind(&mut self, kind: DataKind, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        let EditAction::Create(previous) = editor.action else {
            return;
        };
        if previous == kind {
            return;
        }
        let previous_field = matches!(previous, DataKind::Hash | DataKind::Stream);
        let next_field = matches!(kind, DataKind::Hash | DataKind::Stream);
        if previous_field != next_field || (previous == DataKind::ZSet) != (kind == DataKind::ZSet)
        {
            let extra = if kind == DataKind::ZSet {
                "0".to_owned()
            } else if next_field {
                if editor.encoding == value::EditEncoding::Text {
                    "field".into()
                } else {
                    "\"field\"".into()
                }
            } else {
                String::new()
            };
            editor
                .extra
                .update(cx, |input, cx| input.set_value(extra, cx));
        }
        editor.action = EditAction::Create(kind);
        cx.notify();
    }

    pub(super) fn save_editor(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.native_pending || self.closed {
            return;
        }
        let result = self.editor_command(cx);
        match result {
            Ok(command) => self.mutate(
                command.arguments,
                Some(command.key),
                command.expected,
                command.rejection,
                cx,
            ),
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
            }
        }
    }

    fn editor_command(&self, cx: &App) -> Result<EditCommand, &'static str> {
        let editor = self
            .editor
            .as_ref()
            .ok_or(i18n_key::REDIS_NO_VALUE_IS_BEING_EDITED)?;
        let text = editor.value.read(cx).value();
        let extra = editor.extra.read(cx).value();
        let current = || {
            self.selected_key
                .clone()
                .ok_or(i18n_key::REDIS_SELECT_A_KEY_FIRST)
        };
        let normal = i18n_key::REDIS_THE_KEY_CHANGED_OR_EXPIRED_REFRESH_AND_TRY_AGAIN;
        let mut expected = ExpectedReply::Any;
        let mut rejection = normal;
        let (args, key) = match &editor.action {
            EditAction::Create(kind) => {
                let key = editor.encoding.parse(editor.name.read(cx).value())?;
                let data = editor.encoding.parse(text)?;
                let (command, values) = match kind {
                    DataKind::String => (b"SET".to_vec(), vec![data]),
                    DataKind::Hash => (b"HSET".to_vec(), vec![editor.encoding.parse(extra)?, data]),
                    DataKind::List => (b"RPUSH".to_vec(), vec![data]),
                    DataKind::Set => (b"SADD".to_vec(), vec![data]),
                    DataKind::ZSet => (b"ZADD".to_vec(), vec![score(extra)?, data]),
                    DataKind::Stream => (
                        b"XADD".to_vec(),
                        vec![b"*".to_vec(), editor.encoding.parse(extra)?, data],
                    ),
                };
                // Creation is atomic and refuses to change an existing key. Keep
                // the script constant; all user bytes remain separate arguments.
                let args = if matches!(kind, DataKind::String) {
                    expected = ExpectedReply::Present;
                    vec![command, key.clone(), values[0].clone(), b"NX".to_vec()]
                } else {
                    let mut args = vec![
                        b"EVAL".to_vec(),
                        rustterm::redis::CREATE_SCRIPT.as_bytes().to_vec(),
                        b"1".to_vec(),
                        key.clone(),
                        command,
                    ];
                    args.extend(values);
                    expected = ExpectedReply::Nonzero;
                    args
                };
                rejection = i18n_key::REDIS_THAT_KEY_ALREADY_EXISTS_CHOOSE_A_DIFFERENT_NAME;
                (args, key)
            }
            EditAction::Rename => {
                let key = editor.encoding.parse(editor.name.read(cx).value())?;
                expected = ExpectedReply::Nonzero;
                rejection = i18n_key::REDIS_THAT_KEY_ALREADY_EXISTS_CHOOSE_A_DIFFERENT_NAME;
                (vec![b"RENAMENX".to_vec(), current()?, key.clone()], key)
            }
            EditAction::Ttl => {
                let key = current()?;
                let seconds = text
                    .parse::<i64>()
                    .map_err(|_| i18n_key::REDIS_TTL_MUST_BE_A_POSITIVE_NUMBER_OF_SECONDS_OR_1)?;
                let args = if seconds == -1 {
                    vec![b"PERSIST".to_vec(), key.clone()]
                } else if seconds > 0 {
                    let ms = seconds
                        .checked_mul(1000)
                        .ok_or(i18n_key::REDIS_TTL_IS_TOO_LARGE)?;
                    expected = ExpectedReply::Nonzero;
                    vec![
                        b"PEXPIRE".to_vec(),
                        key.clone(),
                        ms.to_string().into_bytes(),
                    ]
                } else {
                    return Err(i18n_key::REDIS_TTL_MUST_BE_A_POSITIVE_NUMBER_OF_SECONDS_OR_1);
                };
                (args, key)
            }
            EditAction::String => {
                let key = current()?;
                let Some(KeyInfo {
                    data: KeyData::String(original),
                    ..
                }) = &self.info
                else {
                    return Err(normal);
                };
                expected = ExpectedReply::Nonzero;
                (
                    vec![
                        b"EVAL".to_vec(),
                        rustterm::redis::SAVE_STRING_SCRIPT.as_bytes().to_vec(),
                        b"1".to_vec(),
                        key.clone(),
                        original.clone(),
                        editor.encoding.parse(text)?,
                    ],
                    key,
                )
            }
            EditAction::Hash(field) => {
                let key = current()?;
                let field = match field {
                    Some(field) => field.clone(),
                    None => editor.encoding.parse(extra)?,
                };
                (
                    vec![
                        b"HSET".to_vec(),
                        key.clone(),
                        field,
                        editor.encoding.parse(text)?,
                    ],
                    key,
                )
            }
            EditAction::List(item) => {
                let key = current()?;
                let args = if let Some((index, original)) = item {
                    expected = ExpectedReply::Nonzero;
                    vec![
                        b"EVAL".to_vec(),
                        rustterm::redis::SAVE_LIST_SCRIPT.as_bytes().to_vec(),
                        b"1".to_vec(),
                        key.clone(),
                        index.to_string().into_bytes(),
                        original.clone(),
                        editor.encoding.parse(text)?,
                    ]
                } else {
                    vec![b"RPUSH".to_vec(), key.clone(), editor.encoding.parse(text)?]
                };
                (args, key)
            }
            EditAction::Set => {
                let key = current()?;
                (
                    vec![b"SADD".to_vec(), key.clone(), editor.encoding.parse(text)?],
                    key,
                )
            }
            EditAction::ZSet(member) => {
                let key = current()?;
                let member = match member {
                    Some(member) => member.clone(),
                    None => editor.encoding.parse(text)?,
                };
                (
                    vec![b"ZADD".to_vec(), key.clone(), score(extra)?, member],
                    key,
                )
            }
            EditAction::Stream => {
                let key = current()?;
                (
                    vec![
                        b"XADD".to_vec(),
                        key.clone(),
                        b"*".to_vec(),
                        editor.encoding.parse(extra)?,
                        editor.encoding.parse(text)?,
                    ],
                    key,
                )
            }
        };
        Ok(EditCommand {
            arguments: args,
            key,
            expected,
            rejection,
        })
    }

    pub(super) fn delete_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let Some(key) = self.selected_key.clone() else {
            return;
        };
        self.confirm_delete(
            vec![b"DEL".to_vec(), key],
            None,
            i18n_key::REDIS_DELETE_THIS_REDIS_KEY,
            window,
            cx,
        );
    }

    pub(super) fn delete_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.available(cx) {
            return;
        }
        let (Some(info), Some(key), Some(row)) =
            (&self.info, self.selected_key.clone(), self.selected_row)
        else {
            return;
        };
        let operation = match &info.data {
            KeyData::Hash(rows) => rows
                .get(row)
                .map(|(field, _)| (b"HDEL".as_slice(), field.clone())),
            KeyData::Set(rows) => rows
                .get(row)
                .map(|member| (b"SREM".as_slice(), member.clone())),
            KeyData::ZSet(rows) => rows
                .get(row)
                .map(|(member, _)| (b"ZREM".as_slice(), member.clone())),
            KeyData::Stream(rows) => rows
                .get(row)
                .map(|entry| (b"XDEL".as_slice(), entry.id.clone())),
            _ => None,
        };
        if let Some((command, member)) = operation {
            self.confirm_delete(
                vec![command.to_vec(), key.clone(), member],
                Some(key),
                i18n_key::REDIS_DELETE_THIS_REDIS_ENTRY,
                window,
                cx,
            );
        }
    }

    fn confirm_delete(
        &mut self,
        args: Vec<Vec<u8>>,
        key: Option<Vec<u8>>,
        title: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let generation = self.generation;
        let answer = window.prompt(
            PromptLevel::Warning,
            tr(title),
            Some(tr(i18n_key::REDIS_THIS_CHANGE_CANNOT_BE_UNDONE)),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_DELETE)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = this.update(cx, |this, cx| {
                let confirmed = confirmed && permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if confirmed && generation == this.generation && !this.closed {
                    this.mutate(args, key, ExpectedReply::Any, "", cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn copy_value(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            return;
        }
        let Some(info) = &self.info else { return };
        let bytes = match &info.data {
            KeyData::String(bytes) => Some(bytes.as_slice()),
            KeyData::Hash(rows) => self
                .selected_row
                .and_then(|row| rows.get(row))
                .map(|(_, value)| value.as_slice()),
            KeyData::List(rows) | KeyData::Set(rows) => self
                .selected_row
                .and_then(|row| rows.get(row))
                .map(Vec::as_slice),
            KeyData::ZSet(rows) => self
                .selected_row
                .and_then(|row| rows.get(row))
                .map(|(member, _)| member.as_slice()),
            _ => None,
        };
        if let Some(bytes) = bytes {
            if let Ok(text) = std::str::from_utf8(bytes) {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                let message = if value::partial_string(info, &self.detail_page) {
                    i18n_key::REDIS_PAGE_COPIED
                } else {
                    i18n_key::REDIS_VALUE_COPIED
                };
                self.show_success(message, cx);
            } else {
                self.error = Some(i18n_key::REDIS_BINARY_VALUES_CANNOT_BE_COPIED_AS_TEXT.into());
            }
        }
        cx.notify();
    }
}

fn score(text: &str) -> Result<Vec<u8>, &'static str> {
    let score = text
        .parse::<f64>()
        .map_err(|_| i18n_key::REDIS_ENTER_A_FINITE_NUMERIC_SCORE)?;
    if !score.is_finite() {
        return Err(i18n_key::REDIS_ENTER_A_FINITE_NUMERIC_SCORE);
    }
    Ok(text.as_bytes().to_vec())
}
