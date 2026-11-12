use super::probing;
use super::{LeaveAction, Modal, Workspace};
use crate::app::forms::ProfileKind;
use crate::app::ui;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{AnyElement, Context, MouseButton, Window, div, px, relative, rgb, rgba};
use rustterm::config::Connection;
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

impl Workspace {
    pub(super) fn render_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let modal = self.modal.as_ref()?;
        let (title, body, save_label) = match modal {
            Modal::Tunnels(panel) => (
                tr(i18n_key::CONNECTIONS_SSH_PORT_FORWARDING),
                panel.clone().into_any_element(),
                None,
            ),
            Modal::Help => (
                tr(i18n_key::WORKSPACE_ABOUT_RUSTTERM),
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(gpui::img(ui::APP_ICON).size(px(48.)).flex_shrink_0())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(rustterm::trf!(
                                        i18n_key::WORKSPACE_RUSTTERM_VERSION_LINUX_DESKTOP_TERMINAL,
                                        version = env!("CARGO_PKG_VERSION")
                                    ))
                                    .child(div().text_color(rgb(ui::colors().muted)).child(tr(
                                        i18n_key::WORKSPACE_LOCAL_PTY_SSH_SFTP_SZ_RZ_TABS_AND_SPLIT_PANES,
                                    ))),
                            ),
                    )
                    .children(
                        [
                            (tr(i18n_key::WORKSPACE_NEW_LOCAL_TERMINAL), "Ctrl+Shift+T"),
                            (tr(i18n_key::WORKSPACE_COMMAND_PALETTE), "Ctrl+Shift+P"),
                            (tr(i18n_key::COMMON_SETTINGS_DIALOG), "Ctrl+,"),
                            (tr(i18n_key::SETTINGS_LOCK_RUSTTERM), "Ctrl+Shift+L"),
                            (tr(i18n_key::WORKSPACE_CLOSE_CURRENT_PANE), "Ctrl+Shift+W"),
                            (tr(i18n_key::TERMINAL_RECONNECT_ENDED_SESSION), "Ctrl+Shift+R"),
                            (tr(i18n_key::WORKSPACE_COPY_PASTE), "Ctrl+Shift+C / V"),
                            (tr(i18n_key::WORKSPACE_SEARCH_OUTPUT), "Ctrl+Shift+F"),
                            (tr(i18n_key::WORKSPACE_SFTP_FILE_MANAGER), "Ctrl+Shift+S"),
                            (tr(i18n_key::WORKSPACE_SPLIT_HORIZONTALLY_VERTICALLY), "Ctrl+Shift+D / E"),
                            (tr(i18n_key::WORKSPACE_NEXT_PANE), "Ctrl+Shift+J"),
                            (tr(i18n_key::WORKSPACE_ZOOM_RESTORE_PANE), "Ctrl+Shift+Enter"),
                            (tr(i18n_key::WORKSPACE_SHOW_CONNECTION_SIDEBAR), "Ctrl+Shift+B"),
                            (tr(i18n_key::WORKSPACE_PANE_ACTIONS), "Shift+F10"),
                            (tr(i18n_key::WORKSPACE_NEXT_TAB), "Ctrl+Tab"),
                            (
                                tr(i18n_key::WORKSPACE_INCREASE_DECREASE_RESET_FONT_SIZE),
                                "Ctrl++ / − / 0",
                            ),
                        ]
                        .into_iter()
                        .map(|(label, key)| {
                            div()
                                .flex()
                                .justify_between()
                                .child(label)
                                .child(div().text_color(rgb(ui::colors().muted)).child(key))
                        }),
                    )
                    .child(
                        div()
                            .pt_3()
                            .border_t_1()
                            .border_color(rgb(ui::colors().border))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(tr(i18n_key::TERMINAL_SHELL_SHORTCUTS))
                            .children([
                                (i18n_key::TERMINAL_SHELL_SEARCH_HISTORY, "Ctrl+R"),
                                (i18n_key::TERMINAL_SHELL_KILL_TO_START, "Ctrl+U"),
                                (i18n_key::TERMINAL_SHELL_KILL_TO_END, "Ctrl+K"),
                                (i18n_key::TERMINAL_SHELL_YANK, "Ctrl+Y"),
                                (i18n_key::TERMINAL_SHELL_LINE_START_END, "Ctrl+A / E"),
                            ].into_iter().map(|(label, key)| {
                                div().flex().justify_between().gap_3()
                                    .child(tr(label))
                                    .child(div().flex_shrink_0().text_color(rgb(ui::colors().muted)).child(key))
                            }))
                            .child(div().text_color(rgb(ui::colors().muted)).child(tr(i18n_key::TERMINAL_SHELL_SHORTCUTS_NOTE)))
                            .child(div().text_color(rgb(ui::colors().muted)).child(tr(i18n_key::TERMINAL_SHELL_STATUS_SUPPORT))),
                    )
                    .into_any_element(),
                None,
            ),
            Modal::Profile(form) => (
                if form.read(cx).original.is_some() {
                    tr(i18n_key::WORKSPACE_EDIT_SESSION)
                } else {
                    match form.read(cx).kind {
                        ProfileKind::Ssh => tr(i18n_key::WORKSPACE_ADD_SSH_PROFILE),
                        ProfileKind::Telnet => tr(i18n_key::CONNECTIONS_ADD_TELNET_CONNECTION),
                        ProfileKind::Local => tr(i18n_key::WORKSPACE_ADD_LOCAL_PROFILE),
                        ProfileKind::Redis => tr(i18n_key::REDIS_ADD_REDIS_CONNECTION),
                        ProfileKind::Elasticsearch => tr(i18n_key::ES_ADD_CONNECTION),
                        ProfileKind::Mysql => tr(i18n_key::DATABASE_ADD_MYSQL_CONNECTION),
                        ProfileKind::ClickHouse => tr(i18n_key::DATABASE_ADD_CLICKHOUSE_CONNECTION),
                        ProfileKind::Postgres => tr(i18n_key::DATABASE_ADD_POSTGRESQL_CONNECTION),
                        ProfileKind::Sqlite => tr(i18n_key::DATABASE_ADD_SQLITE_DATABASE),
                        ProfileKind::Rdp => tr(i18n_key::RDP_ADD_RDP_CONNECTION),
                    }
                },
                form.clone().into_any_element(),
                Some(tr(i18n_key::COMMON_SAVE)),
            ),
            Modal::Settings(form) => (
                tr(i18n_key::COMMON_SETTINGS),
                form.clone().into_any_element(),
                Some(tr(if self.save_failed {
                    i18n_key::WORKSPACE_RETRY_SAVING
                } else {
                    i18n_key::COMMON_SAVE
                })),
            ),
            Modal::Group { fields, parent, .. } => {
                let path = rustterm::config::Group::tree(&self.config.groups).into_iter()
                    .find(|node| Some(node.group.id) == *parent).map(|node| node.path)
                    .unwrap_or_else(|| tr(i18n_key::WORKSPACE_GROUP_ROOT).to_owned());
                (tr(i18n_key::WORKSPACE_GROUP), div().flex().flex_col().gap_3()
                    .child(div().text_color(rgb(ui::colors().muted)).child(format!("{}: {path}", tr(i18n_key::WORKSPACE_GROUP_PARENT))))
                    .child(fields.rows()).into_any_element(), Some(tr(i18n_key::COMMON_SAVE)))
            },
            Modal::Auth { auto_connect: true, .. } => (
                tr(i18n_key::WORKSPACE_CONNECTION_AUTHENTICATION),
                div().child(tr(i18n_key::COMMON_LOADING)).into_any_element(),
                None,
            ),
            Modal::Auth {
                profile,
                secret,
                sentinel_secret,
                remember,
                tunnel,
                ..
            } => {
                let target = match &profile.connection {
                    Connection::Ssh(s) => format!("{}@{}:{}", s.username, s.host, s.port),
                    Connection::Redis(s) => {
                        format!("Redis · {}:{} · DB {}", s.host, s.port, s.database)
                    }
                    Connection::Elasticsearch(s) => format!("Elasticsearch · {}", s.endpoint),
                    Connection::Rdp(s) => format!("RDP · {}@{}:{}", s.username, s.host, s.port),
                    Connection::Mysql(s) | Connection::Postgres(s) | Connection::ClickHouse(s) => format!(
                        "{} · {}@{}:{} / {}",
                        profile.connection.sql().unwrap().0.label(),
                        s.username,
                        s.host,
                        s.port,
                        s.database
                    ),
                    _ => String::new(),
                };
                (
                    tr(i18n_key::WORKSPACE_CONNECTION_AUTHENTICATION),
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(target)
                        .child(secret.clone())
                        .when_some(sentinel_secret.as_ref(), |body, secret| body.child(tr(i18n_key::WORKSPACE_SENTINEL_PASSWORD_THIS_CONNECTION_ONLY)).child(secret.clone()))
                        .child(tr(match profile.connection {
                            Connection::Elasticsearch(_) => i18n_key::ES_AUTH_HELP,
                            Connection::Redis(_) => i18n_key::REDIS_AUTHENTICATION_IS_SKIPPED_ONLY_WHEN_BOTH_USERNAME_AND_PASSWORD_ARE,
                            Connection::Mysql(_) | Connection::Postgres(_) | Connection::ClickHouse(_) => i18n_key::DATABASE_ENTER_THE_DATABASE_PASSWORD_TLS_VERIFIES_THE_SERVER_CERTIFICATE_AND,
                            Connection::Rdp(_) => i18n_key::RDP_RDP_USES_TLS_AND_NETWORK_LEVEL_AUTHENTICATION_NLA_CREDENTIALS_ARE,
                            _ => i18n_key::WORKSPACE_LEAVE_BLANK_FOR_AN_UNENCRYPTED_PRIVATE_KEY_AUTHENTICATION_CREDENTIALS_ARE,
                        }))
                        .when(self.can_save_connection_credential(profile), |body| body.child(
                            div()
                                .id("remember-secret")
                                .cursor_pointer()
                                .tab_index(0)
                                .focus(|style| style.bg(rgb(ui::colors().selected)))
                                .child(rustterm::trf!(
                                    i18n_key::WORKSPACE_MARK_SAVE_IN_THE_SYSTEM_CREDENTIAL_STORE_USE_ONCE_IF,
                                    mark = if *remember { "✓" } else { " " }
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(Modal::Auth { remember, .. }) = &mut this.modal
                                    {
                                        *remember = !*remember;
                                    }
                                    cx.notify();
                                })),
                        ))
                        .when(!self.can_save_connection_credential(profile), |body| body.child(tr(
                            i18n_key::DATABASE_THIS_DATABASE_SELECTION_APPLIES_TO_THIS_PANE_CREDENTIALS_ARE_USED,
                        )))
                        .when_some(tunnel.as_ref(), |content, tunnel| {
                            let Connection::Ssh(ssh) = &tunnel.profile.connection else { unreachable!("validated SSH route") };
                            content.child(div().w_full().min_w_0().border_t_1().border_color(rgb(ui::colors().border)).pt_3()
                                .flex().flex_col().gap_3()
                                .child(rustterm::trf!(i18n_key::REDIS_SSH_TUNNEL_VIA_NAME_USER_HOST_PORT,
                                    name = &tunnel.profile.name, user = &ssh.username, host = &ssh.host, port = ssh.port))
                                .child(tunnel.secret.clone())
                                .child(tr(i18n_key::WORKSPACE_LEAVE_BLANK_FOR_AN_UNENCRYPTED_PRIVATE_KEY_AUTHENTICATION_CREDENTIALS_ARE))
                                .child(div().id("remember-tunnel-secret").cursor_pointer().tab_index(0)
                                    .focus(|style| style.bg(rgb(ui::colors().selected)))
                                    .child(rustterm::trf!(i18n_key::REDIS_MARK_SAVE_THE_SSH_CREDENTIAL_IN_THE_SYSTEM_CREDENTIAL_STORE,
                                        mark = if tunnel.remember { "✓" } else { " " }))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(Modal::Auth { tunnel: Some(tunnel), .. }) = &mut this.modal {
                                            tunnel.remember = !tunnel.remember;
                                        }
                                        cx.notify();
                                    }))))
                        })
                        .into_any_element(),
                    Some(tr(i18n_key::COMMON_CONNECT)),
                )
            }
            Modal::Loading => (
                tr(i18n_key::WORKSPACE_LOADING_HOST_TRUST),
                div().child(tr(i18n_key::COMMON_LOADING)).into_any_element(),
                None,
            ),
            Modal::Trust(hosts) => {
                let mut content=div().flex().flex_col().gap_3().child(tr(i18n_key::WORKSPACE_A_CHANGED_HOST_KEY_BLOCKS_AUTHENTICATION_VERIFY_THE_SERVER_CHANGE));
                for (index, record) in hosts.entries().iter().enumerate() {
                    let host = record.host.clone();
                    let port = record.port;
                    let keys = record
                        .keys
                        .iter()
                        .map(|key| {
                            format!(
                                "{} {}",
                                key.algorithm,
                                key.fingerprint()
                                    .unwrap_or_else(|_| tr(i18n_key::WORKSPACE_INVALID_PUBLIC_KEY).into())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let expected_keys = record.keys.clone();
                    content = content.child(
                        div()
                            .p_2()
                            .bg(rgb(ui::colors().raised))
                            .child(format!("{}:{}\n{}", record.host, port, keys))
                            .child(
                                div()
                                    .id(("forget-host", index))
                                    .cursor_pointer()
                                    .text_color(rgb(ui::colors().danger))
                                    .child(tr(i18n_key::WORKSPACE_DELETE_THIS_TRUST_RECORD))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.forget_host(
                                            host.clone(),
                                            port,
                                            expected_keys.clone(),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    );
                }
                (tr(i18n_key::WORKSPACE_SSH_HOST_TRUST), content.into_any_element(), None)
            }
        };
        let settings = matches!(modal, Modal::Settings(_));
        let ports = matches!(modal, Modal::Tunnels(_));
        let rdp_profile =
            matches!(modal, Modal::Profile(form) if form.read(cx).kind == ProfileKind::Rdp);
        let settings_dirty = matches!(modal, Modal::Settings(form) if form.read(cx).is_dirty(cx));
        let save_enabled =
            !settings || ((settings_dirty || self.save_failed) && !self.modal_saving);
        let body = if settings || rdp_profile || ports {
            div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .flex_1()
                .overflow_hidden()
                .child(body)
                .into_any_element()
        } else {
            div()
                .id("modal-body")
                .w_full()
                .min_w_0()
                .track_scroll(&self.modal_scroll)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .map(ui::preserve_scroll_axes)
                .child(body)
                .map(|content| ui::scrollable(content, Some(self.modal_scroll.clone())))
                .into_any_element()
        };
        let mut card = ui::dialog_surface()
            .w(px(if settings { 900. } else { 700. }))
            .when(settings, |card| card.h(px(640.)).overflow_hidden())
            .when(rdp_profile, |card| card.h(px(620.)))
            .when(ports, |card| card.h(px(620.)))
            .max_w(relative(0.95))
            .max_h(relative(0.92))
            .flex()
            .flex_col()
            .when(!settings, |card| card.p_4().gap_4())
            .child(
                div()
                    .flex_shrink_0()
                    .when(settings, |header| {
                        header
                            .flex_shrink_0()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(ui::colors().border))
                    })
                    .text_size(px(ui::metrics::HEADING))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(body);
        if let Some(error) = &self.error {
            card = card.child(
                ui::feedback(error.render(), true)
                    .id("modal-error")
                    .max_h(px(96.))
                    .when(settings, |error| error.mx_4().mb_2())
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .map(|content| ui::scrollable(content, None)),
            );
        }
        if let Some(test) = &self.profile_test {
            card = card.child(
                ui::feedback(test.message(), false)
                    .id("profile-test-result")
                    .max_h(px(96.))
                    .text_color(rgb(test.color()))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .map(|content| ui::scrollable(content, None)),
            );
        }
        card = card.child(
            div()
                .flex_shrink_0()
                .flex()
                .justify_end()
                .gap_3()
                .when(settings, |footer| {
                    footer
                        .items_center()
                        .flex_shrink_0()
                        .px_4()
                        .py_3()
                        .border_t_1()
                        .border_color(rgb(ui::colors().border))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(11.))
                                .text_color(rgb(ui::colors().muted))
                                .when(settings_dirty, |status| status.child(tr(i18n_key::CONNECTIONS_UNSAVED_CHANGES))),
                        )
                })
                .when(matches!(modal, Modal::Profile(form) if form.read(cx).kind != ProfileKind::Local), |footer| {
                    footer
                        .child(ui::text_button("test-profile", if self.profile_test.as_ref().is_some_and(probing::ProfileTest::is_running) {
                            tr(i18n_key::CONNECTIONS_CANCEL_TEST)
                        } else if matches!(modal, Modal::Profile(form) if form.read(cx).kind == ProfileKind::Sqlite) {
                            tr(i18n_key::DATABASE_CHECK_DATABASE_ACCESS)
                        } else {
                            tr(i18n_key::CONNECTIONS_TEST_CONNECTION)
                        }, ui::ButtonStyle::Neutral)
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, window, cx| this.test_profile(window, cx))))
                        .child(div().flex_1())
                })
                .child(
                    ui::text_button_enabled(
                        "cancel-modal",
                        if save_label.is_some() {
                            tr(i18n_key::COMMON_CANCEL)
                        } else {
                            tr(i18n_key::COMMON_CLOSE)
                        },
                        ui::ButtonStyle::Neutral,
                        !settings || !self.modal_saving,
                    )
                    .track_focus(&self.modal_cancel_focus)
                    .tab_index(0)
                    .focus(|style| style.bg(rgb(ui::colors().selected)).border_color(rgb(ui::colors().accent)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.request_leave_modal(LeaveAction::CloseModal, window, cx);
                        cx.notify();
                    })),
                )
                .when_some(save_label, |d, label| {
                    d.child(
                        ui::text_button_enabled("save-modal", label, ui::ButtonStyle::Primary, save_enabled)
                            .track_focus(&self.modal_save_focus)
                            .tab_index(0)
                            .focus(|style| style.border_color(rgb(ui::colors().accent)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if matches!(this.modal, Some(Modal::Auth { .. })) {
                                    this.authenticate(window, cx);
                                } else {
                                    this.save_modal(window, cx);
                                }
                            })),
                    )
                }),
        );
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x000000aa))
                .map(|backdrop| WindowFrame::new(window).rounded(backdrop))
                .occlude()
                .track_focus(&self.modal_focus)
                .tab_group()
                .tab_index(0)
                .tab_stop(false)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .child(card)
                .into_any_element(),
        )
    }
}
