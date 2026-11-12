use super::{
    interaction::WindowState,
    port_editor::{EditorEvent, PortEditor},
    ui,
};
use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Render, RenderImage, Subscription, Task,
    Timer, Window, div, img, prelude::*, px, rgb,
};
use rustterm::{
    config::{
        ForwardKind, PortForward, Uuid,
        ssh::{AutoForwardRule, ForwardProtocol},
    },
    i18n::{messages as key, tr},
    session::{
        DiscoveredService, DiscoveryStatus, ForwardController, ForwardSettings, ForwardState,
        ForwardStatus,
    },
};
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

pub struct TunnelPanel {
    connections: Vec<(String, ForwardController)>,
    selected_connection: usize,
    controller: ForwardController,
    state: WindowState,
    statuses: Vec<ForwardStatus>,
    settings: ForwardSettings,
    discovery: DiscoveryStatus,
    editor: Option<Entity<PortEditor>>,
    editor_subscription: Option<Subscription>,
    qr: Option<(String, Arc<RenderImage>)>,
    error: Option<String>,
    focus: FocusHandle,
    _poll: Task<()>,
}
impl TunnelPanel {
    pub fn new(controller: ForwardController, state: WindowState, cx: &mut Context<Self>) -> Self {
        let statuses = controller.statuses();
        let settings = controller.settings();
        let discovery = controller.discovery();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(500)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.state.is_locked() {
                            return;
                        }
                        let statuses = this.controller.statuses();
                        let settings = this.controller.settings();
                        let discovery = this.controller.discovery();
                        if this.statuses != statuses
                            || this.settings != settings
                            || this.discovery != discovery
                        {
                            this.statuses = statuses;
                            this.settings = settings;
                            this.discovery = discovery;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            connections: Vec::new(),
            selected_connection: 0,
            controller,
            state,
            statuses,
            settings,
            discovery,
            editor: None,
            editor_subscription: None,
            qr: None,
            error: None,
            focus: cx.focus_handle(),
            _poll: poll,
        }
    }
    pub fn connections(
        connections: Vec<(String, ForwardController)>,
        state: WindowState,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::new(connections[0].1.clone(), state, cx);
        panel.connections = connections;
        panel
    }
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub fn update_connections(
        &mut self,
        connections: Vec<(String, ForwardController)>,
        cx: &mut Context<Self>,
    ) {
        if self.connections.len() == connections.len()
            && self
                .connections
                .iter()
                .zip(&connections)
                .all(|(a, b)| a.0 == b.0 && a.1.same_instance(&b.1))
        {
            return;
        }
        let selected = connections
            .iter()
            .position(|(_, controller)| self.controller.same_instance(controller));
        self.selected_connection = selected.unwrap_or(0);
        if selected.is_none() {
            self.controller = connections[0].1.clone();
            self.settings = self.controller.settings();
            self.statuses = self.controller.statuses();
            self.discovery = self.controller.discovery();
            self.editor = None;
            self.editor_subscription = None;
            self.qr = None;
            self.error = None;
        }
        self.connections = connections;
        cx.notify();
    }
    pub fn input_composing(&self, cx: &App) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).input_composing(cx))
    }
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some(editor) = &self.editor {
            editor.read(cx).focus(window, cx);
        } else {
            self.focus.focus(window);
        }
    }
    fn apply(&mut self, settings: ForwardSettings, cx: &mut Context<Self>) {
        if self.state.is_locked() {
            return;
        }
        match self.controller.configure(settings.clone()) {
            Ok(()) => {
                self.settings = settings;
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }
    fn edit(
        &mut self,
        rule: Option<PortForward>,
        service: Option<DiscoveredService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.is_locked() {
            return;
        }
        let editor = cx.new(|cx| match &service {
            Some(service) => PortEditor::automatic(
                service,
                self.settings
                    .automatic
                    .services
                    .iter()
                    .find(|p| p.service == service.identity)
                    .cloned(),
                cx,
            ),
            None => PortEditor::new(rule, cx),
        });
        self.editor_subscription =
            Some(
                cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
                    if this.state.is_locked() {
                        return;
                    }
                    match event {
                        EditorEvent::Save { rule, automatic } => {
                            let mut settings = this.settings.clone();
                            if let Some(policy) = automatic {
                                if let Some(old) = settings
                                    .automatic
                                    .services
                                    .iter_mut()
                                    .find(|p| p.service == policy.service)
                                {
                                    *old = policy.clone();
                                } else {
                                    settings.automatic.services.push(policy.clone());
                                }
                            } else if let Some(old) =
                                settings.rules.iter_mut().find(|old| old.id == rule.id)
                            {
                                *old = rule.as_ref().clone();
                            } else {
                                settings.rules.push(rule.as_ref().clone());
                            }
                            this.apply(settings, cx);
                            if this.error.is_none() {
                                this.editor = None;
                                this.editor_subscription = None;
                                this.focus.focus(window);
                            }
                        }
                        EditorEvent::Cancel => {
                            this.editor = None;
                            this.editor_subscription = None;
                            this.focus.focus(window);
                        }
                        EditorEvent::Changed => {}
                    }
                    cx.notify();
                }),
            );
        editor.read(cx).focus(window, cx);
        self.editor = Some(editor);
        self.qr = None;
        cx.notify();
    }
    fn toggle(&mut self, status: &ForwardStatus, cx: &mut Context<Self>) {
        if self.state.is_locked() {
            return;
        }
        let mut settings = self.settings.clone();
        if let Some(identity) = &status.service {
            if let Some(policy) = settings
                .automatic
                .services
                .iter_mut()
                .find(|p| &p.service == identity)
            {
                policy.enabled = !status.enabled;
            }
        } else if let Some(rule) = settings.rules.iter_mut().find(|r| r.id == status.rule.id) {
            rule.enabled = !status.enabled;
        }
        self.apply(settings, cx);
    }
    fn remove(&mut self, status: &ForwardStatus, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        if let Some(identity) = &status.service {
            // Keep an explicit disabled policy so the next discovery cannot
            // recreate a rule that the user deliberately removed.
            if let Some(policy) = settings
                .automatic
                .services
                .iter_mut()
                .find(|p| &p.service == identity)
            {
                policy.enabled = false;
            }
        } else {
            settings.rules.retain(|r| r.id != status.rule.id);
        }
        self.apply(settings, cx);
    }
    fn forward(&mut self, service: &DiscoveredService, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        if settings.automatic.enabled && (service.owned || service.project.is_some()) {
            if let Some(policy) = settings
                .automatic
                .services
                .iter_mut()
                .find(|p| p.service == service.identity)
            {
                policy.enabled = true;
            } else {
                settings.automatic.services.push(AutoForwardRule {
                    service: service.identity.clone(),
                    enabled: true,
                    bind_address: Ipv4Addr::LOCALHOST.into(),
                    local_port: None,
                    last_port: None,
                    protocol: ForwardProtocol::Tcp,
                });
            }
        } else {
            settings.rules.push(PortForward {
                id: Uuid::new_v4(),
                name: service.name.clone(),
                protocol: ForwardProtocol::Tcp,
                kind: ForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: service.port,
                destination_host: service.host.clone(),
                destination_port: service.port,
                enabled: true,
            });
        }
        self.apply(settings, cx);
    }
    fn project(&mut self, project: &str, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        let enabled = !settings.automatic.projects.iter().any(|p| p == project);
        if enabled {
            settings.automatic.enabled = true;
            settings.automatic.projects.push(project.to_owned());
        } else {
            settings.automatic.projects.retain(|p| p != project);
        }
        for policy in &mut settings.automatic.services {
            if matches!(&policy.service,rustterm::config::ssh::ServiceIdentity::Compose { project:p,.. } if p==project)
            {
                policy.enabled = enabled;
            }
        }
        self.apply(settings, cx);
        let _ = self.controller.refresh();
    }
}

fn address(status: &ForwardStatus) -> Option<(String, bool, bool)> {
    if status.rule.kind == ForwardKind::Remote {
        return None;
    }
    let mut address = status.local_address?;
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    let scheme = if status.rule.kind == ForwardKind::Dynamic {
        None
    } else {
        match status.rule.protocol {
            ForwardProtocol::Tcp => None,
            ForwardProtocol::Http => Some("http"),
            ForwardProtocol::Https => Some("https"),
        }
    };
    Some((
        scheme.map_or_else(
            || address.to_string(),
            |scheme| format!("{scheme}://{address}"),
        ),
        scheme.is_some(),
        !address.ip().is_loopback(),
    ))
}
fn qr_image(text: &str) -> Option<Arc<RenderImage>> {
    let qr = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()?;
    let side = (qr.size() + 8) as u32;
    let buffer = image::RgbaImage::from_fn(side * 5, side * 5, |x, y| {
        let dark = qr.get_module((x / 5) as i32 - 4, (y / 5) as i32 - 4);
        image::Rgba(if dark {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        })
    });
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

impl Render for TunnelPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("ports-panel")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        ui::text_button(
                            "ports-auto",
                            key::PORTS_AUTO,
                            if self.settings.automatic.enabled {
                                ui::ButtonStyle::Primary
                            } else {
                                ui::ButtonStyle::Neutral
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut settings = this.settings.clone();
                            settings.automatic.enabled = !settings.automatic.enabled;
                            this.apply(settings, cx);
                        })),
                    )
                    .child(
                        ui::icon_button("ports-discover", ui::Icon::Refresh, key::PORTS_REFRESH)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.state.is_locked() {
                                    this.error =
                                        this.controller.refresh().err().map(|e| e.to_string());
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        ui::text_button("ports-add", key::PORTS_ADD, ui::ButtonStyle::Neutral)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.edit(None, None, window, cx)
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(key::PORTS_LOCAL_DEFAULT)),
            );
        if !self.connections.is_empty() {
            body =
                body.child(
                    div().flex().flex_wrap().gap_1().children(
                        self.connections
                            .iter()
                            .enumerate()
                            .map(|(index, (name, _))| {
                                div()
                                    .id(("ports-connection", index))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .tab_index(0)
                                    .bg(rgb(if self.selected_connection == index {
                                        ui::colors().selected
                                    } else {
                                        ui::colors().surface
                                    }))
                                    .child(name.clone())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.state.is_locked() {
                                            return;
                                        }
                                        this.selected_connection = index;
                                        this.controller = this.connections[index].1.clone();
                                        this.settings = this.controller.settings();
                                        this.statuses = this.controller.statuses();
                                        this.discovery = this.controller.discovery();
                                        this.editor = None;
                                        this.editor_subscription = None;
                                        this.qr = None;
                                        this.error = None;
                                        cx.notify();
                                    }))
                            }),
                    ),
                );
        }
        if let Some(editor) = &self.editor {
            body = body.child(editor.clone());
        }
        if let Some((url, image)) = &self.qr {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .bg(rgb(0xffffff))
                    .child(img(image.clone()).size(px(image.size(0).width.0 as f32)))
                    .child(div().text_color(rgb(0x222222)).text_xs().child(url.clone()))
                    .child(
                        div()
                            .text_color(rgb(0x555555))
                            .text_xs()
                            .child(tr(key::PORTS_QR_HINT)),
                    ),
            );
        }
        if self.statuses.is_empty() {
            body = body.child(
                div()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(key::PORTS_EMPTY)),
            );
        }
        for (index, status) in self.statuses.iter().enumerate() {
            let label = match &status.state {
                ForwardState::Starting => tr(key::CONNECTIONS_STARTING).to_owned(),
                ForwardState::Running => tr(key::PORTS_LISTENING).to_owned(),
                ForwardState::Stopping => tr(key::CONNECTIONS_STOPPING).to_owned(),
                ForwardState::Stopped => tr(key::COMMON_STOPPED).to_owned(),
                ForwardState::Waiting => tr(key::PORTS_WAITING).to_owned(),
                ForwardState::Failed(error) => error.clone(),
            };
            let name = if status.rule.name.is_empty() {
                status.rule.description()
            } else {
                status.rule.name.clone()
            };
            let id = status.rule.id;
            let toggle = status.clone();
            let remove = status.clone();
            let edit = status.clone();
            let mut row = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_1()
                .rounded_md()
                .border_color(rgb(ui::colors().border))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(
                                    if matches!(status.state, ForwardState::Failed(_)) {
                                        ui::colors().danger
                                    } else {
                                        ui::colors().muted
                                    },
                                ))
                                .child(label),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(ui::colors().muted))
                        .child(format!(
                            "{} → {}",
                            rustterm::config::ssh::endpoint(
                                &status.rule.bind_host,
                                status
                                    .local_address
                                    .map_or(status.rule.bind_port, |a| a.port())
                            ),
                            rustterm::config::ssh::endpoint(
                                &status.rule.destination_host,
                                status.rule.destination_port
                            )
                        )),
                );
            let mut actions = div()
                .flex()
                .flex_wrap()
                .gap_1()
                .child(
                    ui::text_button(
                        ("port-toggle", index),
                        if status.enabled {
                            key::PORTS_STOP
                        } else {
                            key::PORTS_START
                        },
                        ui::ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle, cx))),
                )
                .child(
                    ui::icon_button(("port-edit", index), ui::Icon::Edit, key::PORTS_EDIT)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let service = edit
                                .service
                                .as_ref()
                                .and_then(|id| {
                                    this.discovery.services.iter().find(|s| &s.identity == id)
                                })
                                .cloned()
                                .or_else(|| {
                                    edit.service.clone().map(|identity| DiscoveredService {
                                        identity,
                                        name: edit.rule.name.clone(),
                                        project: None,
                                        host: edit.rule.destination_host.clone(),
                                        port: edit.rule.destination_port,
                                        owned: false,
                                        published: edit.available,
                                    })
                                });
                            if edit.service.is_none() || service.is_some() {
                                this.edit(Some(edit.rule.clone()), service, window, cx);
                            }
                        })),
                );
            if status.service.is_none() {
                actions = actions.child(
                    ui::icon_button(("port-remove", index), ui::Icon::Trash, key::PORTS_REMOVE)
                        .on_click(cx.listener(move |this, _, _, cx| this.remove(&remove, cx))),
                );
            }
            if matches!(status.state, ForwardState::Failed(_)) {
                actions = actions.child(
                    ui::icon_button(("port-retry", index), ui::Icon::Refresh, key::PORTS_RETRY)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.state.is_locked() {
                                this.error =
                                    this.controller.retry_rule(id).err().map(|e| e.to_string());
                                cx.notify();
                            }
                        })),
                );
            }
            if let Some((url, browser, lan)) = address(status) {
                row = row.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(ui::colors().accent))
                        .child(url.clone()),
                );
                let copy = url.clone();
                actions = actions.child(
                    ui::icon_button(("port-copy", index), ui::Icon::Copy, key::PORTS_COPY)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.state.is_locked() {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            }
                        })),
                );
                if browser {
                    let open = url.clone();
                    actions = actions.child(
                        ui::icon_button(("port-open", index), ui::Icon::Link, key::PORTS_OPEN)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.state.is_locked() {
                                    cx.open_url(&open);
                                }
                            })),
                    );
                }
                if browser && lan {
                    actions = actions.child(
                        ui::text_button(
                            ("port-qr", index),
                            key::PORTS_QR,
                            ui::ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.state.is_locked() {
                                this.qr = qr_image(&url).map(|image| (url.clone(), image));
                                cx.notify();
                            }
                        })),
                    );
                }
            }
            if let Some(error) = &status.target_error {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(rgb(ui::colors().danger))
                        .child(format!("{}: {error}", tr(key::PORTS_TARGET_UNAVAILABLE))),
                );
            }
            body = body.child(row.child(actions));
        }
        if !self.discovery.services.is_empty() {
            body = body.child(
                div()
                    .mt_2()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(key::PORTS_CANDIDATES)),
            );
        }
        let mut last_project = None;
        for (index, service) in self.discovery.services.iter().enumerate() {
            if service.project != last_project {
                last_project = service.project.clone();
                if let Some(project) = &service.project {
                    let project = project.clone();
                    let enabled = self.settings.automatic.projects.contains(&project);
                    body = body.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .mt_2()
                            .child(project.clone())
                            .child(
                                ui::text_button(
                                    ("port-project", index),
                                    if enabled {
                                        key::PORTS_PROJECT_STOP
                                    } else {
                                        key::PORTS_PROJECT_AUTO
                                    },
                                    ui::ButtonStyle::Neutral,
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.project(&project, cx)),
                                ),
                            ),
                    );
                }
            }
            if self.statuses.iter().any(|status| {
                status.service.as_ref() == Some(&service.identity)
                    || status.rule.kind == ForwardKind::Local
                        && status.rule.destination_host == service.host
                        && status.rule.destination_port == service.port
            }) {
                continue;
            }
            let start = service.clone();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(div().flex_1().min_w_0().truncate().child(format!(
                        "{} · {}",
                        service.name,
                        rustterm::config::ssh::endpoint(&service.host, service.port)
                    )))
                    .child(if service.published {
                        ui::text_button(
                            ("port-discovered", index),
                            key::PORTS_START,
                            ui::ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.forward(&start, cx)))
                        .into_any_element()
                    } else {
                        div()
                            .text_xs()
                            .text_color(rgb(ui::colors().muted))
                            .child(tr(key::PORTS_UNPUBLISHED))
                            .into_any_element()
                    }),
            );
        }
        for error in [
            &self.error,
            &self.discovery.error,
            &self.discovery.docker_error,
        ]
        .into_iter()
        .flatten()
        {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(ui::colors().danger))
                    .child(error.clone()),
            );
        }
        body.child(
            div()
                .mt_2()
                .text_xs()
                .text_color(rgb(ui::colors().muted))
                .child(tr(key::PORTS_BACKGROUND)),
        )
    }
}
