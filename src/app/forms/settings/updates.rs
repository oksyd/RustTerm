use super::*;
use crate::app::updates::Status;
use rustterm::update;

impl SettingsForm {
    pub(super) fn updates_page(&self, cx: &Context<Self>) -> Div {
        let updates = self.updates.read(cx);
        let busy = updates.busy();
        let installed = matches!(updates.status, Status::Installed);
        let mut actions = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                ui::text_button_enabled(
                    "check-updates",
                    i18n_key::UPDATES_CHECK,
                    ui::ButtonStyle::Neutral,
                    !busy && !installed,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let proxy = this
                        .fields
                        .get(i18n_key::UPDATES_PROXY, cx)
                        .trim()
                        .to_owned();
                    this.updates
                        .update(cx, |updates, cx| updates.check(proxy, cx));
                })),
            )
            .child(
                ui::text_button(
                    "view-releases",
                    i18n_key::UPDATES_RELEASES,
                    ui::ButtonStyle::Neutral,
                )
                .on_click(|_, _, cx| cx.open_url(update::RELEASES_URL)),
            );
        if updates.release.is_some() && !installed {
            actions = actions.child(
                ui::text_button_enabled(
                    "install-update",
                    i18n_key::UPDATES_INSTALL,
                    ui::ButtonStyle::Primary,
                    !busy && updates.installation.is_some(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let proxy = this
                        .fields
                        .get(i18n_key::UPDATES_PROXY, cx)
                        .trim()
                        .to_owned();
                    this.updates
                        .update(cx, |updates, cx| updates.install(proxy, cx));
                })),
            );
        }
        if updates.can_cancel() {
            actions = actions.child(
                ui::text_button(
                    "cancel-update",
                    i18n_key::UPDATES_CANCEL,
                    ui::ButtonStyle::Neutral,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.updates.update(cx, |updates, cx| updates.cancel(cx))
                })),
            );
        }
        if installed {
            actions = actions.child(
                ui::text_button_enabled(
                    "restart-update",
                    i18n_key::UPDATES_RESTART,
                    ui::ButtonStyle::Primary,
                    !self.is_dirty(cx),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(RestartForUpdate))),
            );
        }
        column()
            .child(div().pt_4().text_size(px(14.)).child(rustterm::trf!(
                i18n_key::UPDATES_CURRENT_VERSION,
                version = update::CURRENT_VERSION
            )))
            .child(
                toggle(
                    "auto-check-updates",
                    i18n_key::UPDATES_AUTO_CHECK,
                    Some(i18n_key::UPDATES_AUTO_CHECK_HELP),
                    self.values.check_updates,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.check_updates = !this.values.check_updates;
                    cx.notify();
                })),
            )
            .child(section_heading(i18n_key::UPDATES_PROXY))
            .child(self.fields.field(i18n_key::UPDATES_PROXY).clone())
            .child(hint(i18n_key::UPDATES_PROXY_HELP))
            .child(
                div()
                    .mt_4()
                    .p_3()
                    .rounded(px(4.))
                    .bg(rgb(ui::colors().input))
                    .text_color(rgb(if updates.error.is_some() {
                        ui::colors().danger
                    } else {
                        ui::colors().text
                    }))
                    .child(updates.message().render()),
            )
            .child(actions)
            .when(
                updates
                    .installation
                    .as_ref()
                    .is_some_and(update::Installation::is_debian),
                |body| body.child(hint(i18n_key::UPDATES_DEBIAN_HELP)),
            )
            .when(updates.installation.is_none(), |body| {
                body.child(hint(if cfg!(windows) {
                    i18n_key::UPDATES_WINDOWS_PORTABLE
                } else {
                    i18n_key::UPDATES_UNMANAGED
                }))
            })
            .when(installed, |body| {
                body.child(hint(if self.is_dirty(cx) {
                    i18n_key::UPDATES_SAVE_BEFORE_RESTART
                } else {
                    i18n_key::UPDATES_RESTART_HELP
                }))
            })
    }
}
