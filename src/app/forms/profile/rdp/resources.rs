use super::*;
use rustterm::i18n::messages as i18n_key;

impl ProfileForm {
    pub(super) fn rdp_resources(&self, cx: &mut Context<Self>) -> AnyElement {
        section(i18n_key::RDP_LOCAL_RESOURCES)
            .child(self.rdp_choices(
                "rdp-clipboard",
                i18n_key::CONNECTIONS_CLIPBOARD,
                self.rdp.clipboard_mode(),
                [
                    (ClipboardMode::Off, i18n_key::COMMON_OFF),
                    (ClipboardMode::Text, i18n_key::RDP_TEXT_ONLY),
                    (ClipboardMode::Files, i18n_key::RDP_TEXT_AND_FILES),
                ],
                |this, mode, _, _| {
                    this.rdp.settings.clipboard = mode != ClipboardMode::Off;
                    this.rdp.settings.clipboard_files = mode == ClipboardMode::Files;
                },
                cx,
            ))
            .child(self.rdp_toggle(
                "rdp-audio-output",
                i18n_key::RDP_PLAY_REMOTE_AUDIO_ON_THIS_COMPUTER,
                self.rdp.settings.audio_output,
                |settings| settings.audio_output = !settings.audio_output,
                cx,
            ))
            .child(self.rdp_toggle(
                "rdp-audio-input",
                i18n_key::RDP_ALLOW_REMOTE_APPLICATIONS_TO_USE_THE_LOCAL_MICROPHONE,
                self.rdp.settings.audio_input,
                |settings| settings.audio_input = !settings.audio_input,
                cx,
            ))
            .child(self.shared_folders_body(cx))
            .into_any_element()
    }

    fn choose_shared_directory(&mut self, cx: &mut Context<Self>) {
        if self.native_pending || self.rdp.settings.shared_folders.len() >= 16 {
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        self.native_pending = true;
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr(i18n_key::RDP_CHOOSE_A_DIRECTORY_TO_SHARE_WITH_WINDOWS).into()),
        });
        cx.spawn(async move |this, cx| {
            let response = response.await;
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid();
                drop(permit);
                this.native_pending = false;
                if !valid {
                    cx.notify();
                    return;
                }
                this.picker_error = None;
                match response {
                    Ok(Ok(Some(paths))) => {
                        for path in paths.into_iter().take(1) {
                            if this
                                .rdp
                                .settings
                                .shared_folders
                                .iter()
                                .any(|folder| folder.path == path)
                            {
                                continue;
                            }
                            let name = (1..=16).map(|n| format!("Local{n}")).find(|name| {
                                !this
                                    .rdp
                                    .settings
                                    .shared_folders
                                    .iter()
                                    .any(|folder| folder.name.eq_ignore_ascii_case(name))
                            });
                            if let Some(name) = name {
                                this.rdp.settings.shared_folders.push(RdpSharedFolder {
                                    name,
                                    path,
                                    read_only: true,
                                });
                            }
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        this.picker_error =
                            Some(i18n_key::COMMON_COULD_NOT_OPEN_THE_FILE_PICKER_PLEASE_TRY_AGAIN)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn shared_folders_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut body = column().child(hint(
            i18n_key::RDP_SHARED_DIRECTORIES_APPEAR_UNDER_THIS_PC_IN_WINDOWS,
        ));
        for (index, folder) in self.rdp.settings.shared_folders.iter().enumerate() {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().truncate().child(format!(
                        "{}: {}",
                        folder.name,
                        folder.path.display()
                    )))
                    .child(
                        ui::text_button(
                            ("rdp-share-mode", index),
                            if folder.read_only {
                                i18n_key::COMMON_READ_ONLY
                            } else {
                                i18n_key::RDP_READ_AND_WRITE
                            },
                            ButtonStyle::Neutral,
                        )
                        .tab_index(0)
                        .flex_shrink_0()
                        .border_1()
                        .border_color(rgb(ui::colors().border))
                        .focus(|style| style.border_color(rgb(ui::colors().accent)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.native_pending || this.prompts.is_blocked() {
                                return;
                            }
                            if let Some(folder) = this.rdp.settings.shared_folders.get_mut(index) {
                                folder.read_only = !folder.read_only;
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        ui::icon_button(
                            ("rdp-share-remove", index),
                            Icon::Close,
                            i18n_key::RDP_REMOVE_SHARED_DIRECTORY,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.native_pending || this.prompts.is_blocked() {
                                return;
                            }
                            if index < this.rdp.settings.shared_folders.len() {
                                this.rdp.settings.shared_folders.remove(index);
                                cx.notify();
                            }
                        })),
                    ),
            );
        }
        body.child(
            ui::text_button_enabled(
                "rdp-add-share",
                i18n_key::RDP_SHARE_LOCAL_DIRECTORY,
                ButtonStyle::Neutral,
                !self.native_pending && self.rdp.settings.shared_folders.len() < 16,
            )
            .on_click(cx.listener(|this, _, _, cx| this.choose_shared_directory(cx))),
        )
        .into_any_element()
    }
}
