use super::{
    forms::Fields,
    text_input::{InputEvent, TextInput},
    ui,
};
use gpui::{
    App, Context, Entity, EventEmitter, Render, Subscription, Window, div, prelude::*, px, rgb,
};
use rustterm::{
    config::{
        ForwardKind, PortForward, Uuid,
        ssh::{AutoForwardRule, ForwardProtocol},
    },
    i18n::{messages as key, tr},
    session::DiscoveredService,
};
use std::net::IpAddr;

#[derive(Clone)]
pub enum EditorEvent {
    Save {
        rule: Box<PortForward>,
        automatic: Option<AutoForwardRule>,
    },
    Cancel,
    Changed,
}

pub struct PortEditor {
    id: Uuid,
    enabled: bool,
    kind: ForwardKind,
    protocol: ForwardProtocol,
    fields: Fields,
    automatic: Option<AutoForwardRule>,
    interfaces: Vec<(String, IpAddr)>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<EditorEvent> for PortEditor {}

pub fn local_addresses() -> Vec<(String, IpAddr)> {
    let mut addresses = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| {
            let address = interface.ip();
            if address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || matches!(address, IpAddr::V6(ip) if ip.is_unicast_link_local())
            {
                return None;
            }
            Some((interface.name, address))
        })
        .take(64)
        .collect::<Vec<_>>();
    addresses.sort_by(|a, b| (a.1.is_ipv6(), &a.0, a.1).cmp(&(b.1.is_ipv6(), &b.0, b.1)));
    addresses.dedup();
    addresses
}

impl PortEditor {
    pub fn new(rule: Option<PortForward>, cx: &mut Context<Self>) -> Self {
        let rule = rule.unwrap_or(PortForward {
            id: Uuid::new_v4(),
            name: String::new(),
            protocol: ForwardProtocol::Tcp,
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1".into(),
            bind_port: 8080,
            destination_host: "127.0.0.1".into(),
            destination_port: 8080,
            enabled: true,
        });
        let fields = Fields::new(
            vec![
                (key::PORTS_NAME, rule.name),
                (key::PORTS_BIND, rule.bind_host),
                (key::PORTS_LOCAL_PORT, rule.bind_port.to_string()),
                (key::PORTS_TARGET, rule.destination_host),
                (key::PORTS_TARGET_PORT, rule.destination_port.to_string()),
            ],
            cx,
        );
        let subscriptions = fields
            .inputs()
            .iter()
            .map(|input| {
                cx.subscribe(input, |_, _, event, cx| {
                    if matches!(event, InputEvent::Changed) {
                        cx.emit(EditorEvent::Changed);
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            id: rule.id,
            enabled: rule.enabled,
            kind: rule.kind,
            protocol: rule.protocol,
            fields,
            automatic: None,
            interfaces: local_addresses(),
            error: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn automatic(
        service: &DiscoveredService,
        policy: Option<AutoForwardRule>,
        cx: &mut Context<Self>,
    ) -> Self {
        let policy = policy.unwrap_or(AutoForwardRule {
            service: service.identity.clone(),
            enabled: true,
            bind_address: "127.0.0.1".parse().unwrap(),
            local_port: None,
            last_port: None,
            protocol: ForwardProtocol::Tcp,
        });
        let mut editor = Self::new(
            Some(PortForward {
                id: Uuid::new_v4(),
                name: service.name.clone(),
                protocol: policy.protocol,
                kind: ForwardKind::Local,
                bind_host: policy.bind_address.to_string(),
                bind_port: policy.local_port.unwrap_or(service.port),
                destination_host: service.host.clone(),
                destination_port: service.port,
                enabled: policy.enabled,
            }),
            cx,
        );
        if policy.local_port.is_none() {
            editor
                .fields
                .field(key::PORTS_LOCAL_PORT)
                .update(cx, |input, cx| input.clear(cx));
        }
        editor.automatic = Some(policy);
        editor
    }

    pub fn inputs(&self) -> Vec<Entity<TextInput>> {
        let mut keys = vec![key::PORTS_BIND, key::PORTS_LOCAL_PORT];
        if self.automatic.is_none() {
            keys.insert(0, key::PORTS_NAME);
            if self.kind != ForwardKind::Dynamic {
                keys.extend([key::PORTS_TARGET, key::PORTS_TARGET_PORT]);
            }
        }
        keys.into_iter()
            .map(|key| self.fields.field(key).clone())
            .collect()
    }
    pub fn draft(&self, cx: &App) -> Vec<String> {
        let mut result = self
            .fields
            .inputs()
            .iter()
            .map(|input| input.read(cx).value().to_owned())
            .collect::<Vec<_>>();
        result.push(format!(
            "{:?}:{:?}:{}",
            self.kind, self.protocol, self.enabled
        ));
        result
    }
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some(input) = self.inputs().first() {
            input.read(cx).focus_handle().focus(window);
        }
    }
    pub fn input_composing(&self, cx: &App) -> bool {
        self.inputs()
            .iter()
            .any(|input| input.read(cx).is_composing())
    }

    fn value(&self, cx: &App) -> anyhow::Result<(PortForward, Option<AutoForwardRule>)> {
        let get = |key| self.fields.get(key, cx).trim().to_owned();
        let automatic_port = self.automatic.is_some() && get(key::PORTS_LOCAL_PORT).is_empty();
        let bind_port = if automatic_port {
            1
        } else {
            get(key::PORTS_LOCAL_PORT).parse::<u16>()?
        };
        let rule = PortForward {
            id: self.id,
            name: get(key::PORTS_NAME),
            protocol: if self.kind == ForwardKind::Dynamic {
                ForwardProtocol::Tcp
            } else {
                self.protocol
            },
            kind: self.kind,
            bind_host: get(key::PORTS_BIND),
            bind_port,
            destination_host: get(key::PORTS_TARGET),
            destination_port: if self.kind == ForwardKind::Dynamic {
                0
            } else {
                get(key::PORTS_TARGET_PORT).parse::<u16>()?
            },
            enabled: self.enabled,
        };
        rule.validate()?;
        let automatic = self
            .automatic
            .as_ref()
            .map(|original| {
                let address: IpAddr = rule
                    .bind_host
                    .parse()
                    .map_err(|_| anyhow::anyhow!(tr(key::PORTS_INVALID_ADDRESS)))?;
                anyhow::ensure!(
                    !address.is_unspecified() && !address.is_multicast(),
                    tr(key::PORTS_INVALID_ADDRESS)
                );
                anyhow::ensure!(
                    address.is_loopback() || self.interfaces.iter().any(|(_, ip)| *ip == address),
                    tr(key::PORTS_INVALID_ADDRESS)
                );
                Ok::<_, anyhow::Error>(AutoForwardRule {
                    service: original.service.clone(),
                    enabled: original.enabled,
                    bind_address: address,
                    local_port: (!automatic_port).then_some(bind_port),
                    last_port: original.last_port,
                    protocol: self.protocol,
                })
            })
            .transpose()?;
        Ok((rule, automatic))
    }
}

impl Render for PortEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_1()
            .rounded_md()
            .border_color(rgb(ui::colors().border));
        if self.automatic.is_none() {
            body = body.child(
                ui::text_button(
                    "port-enabled",
                    key::PORTS_ENABLED,
                    if self.enabled {
                        ui::ButtonStyle::Primary
                    } else {
                        ui::ButtonStyle::Neutral
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.enabled = !this.enabled;
                    cx.emit(EditorEvent::Changed);
                    cx.notify();
                })),
            );
            body = body.child(
                div().flex().flex_wrap().gap_1().children(
                    [
                        (ForwardKind::Local, key::PORTS_LOCAL),
                        (ForwardKind::Remote, key::PORTS_REMOTE),
                        (ForwardKind::Dynamic, key::PORTS_DYNAMIC),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (kind, label))| {
                        ui::text_button(
                            ("port-kind", index),
                            label,
                            if self.kind == kind {
                                ui::ButtonStyle::Primary
                            } else {
                                ui::ButtonStyle::Neutral
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.kind = kind;
                            cx.emit(EditorEvent::Changed);
                            cx.notify();
                        }))
                    }),
                ),
            );
        }
        if self.automatic.is_none() {
            body = body.child(ui::form_row(
                tr(key::PORTS_NAME),
                self.fields.field(key::PORTS_NAME).clone(),
            ));
        } else {
            body = body.child(div().text_sm().child(format!(
                "{} · {}:{}",
                self.fields.get(key::PORTS_NAME, cx),
                self.fields.get(key::PORTS_TARGET, cx),
                self.fields.get(key::PORTS_TARGET_PORT, cx)
            )));
        }
        if self.kind != ForwardKind::Remote {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        ui::text_button(
                            "port-loopback",
                            key::PORTS_LOOPBACK,
                            ui::ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.fields
                                .field(key::PORTS_BIND)
                                .update(cx, |input, cx| input.set_value("127.0.0.1", cx))
                        })),
                    )
                    .children(
                        self.interfaces
                            .iter()
                            .enumerate()
                            .map(|(index, (name, ip))| {
                                let address = ip.to_string();
                                div()
                                    .id(("port-interface", index))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(ui::colors().border))
                                    .text_size(px(11.))
                                    .cursor_pointer()
                                    .tab_index(0)
                                    .child(format!("{name} · {ip}"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.fields
                                            .field(key::PORTS_BIND)
                                            .update(cx, |input, cx| input.set_value(&address, cx))
                                    }))
                            }),
                    ),
            );
        }
        body = body
            .child(ui::form_row(
                tr(key::PORTS_BIND),
                self.fields.field(key::PORTS_BIND).clone(),
            ))
            .child(ui::form_row(
                tr(if self.kind == ForwardKind::Remote {
                    key::PORTS_REMOTE_PORT
                } else {
                    key::PORTS_LOCAL_PORT
                }),
                self.fields.field(key::PORTS_LOCAL_PORT).clone(),
            ));
        if self.automatic.is_some() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(key::PORTS_AUTO_PORT)),
            );
        }
        if self.kind != ForwardKind::Dynamic && self.automatic.is_none() {
            body = body
                .child(ui::form_row(
                    tr(if self.kind == ForwardKind::Remote {
                        key::PORTS_LOCAL_TARGET
                    } else {
                        key::PORTS_TARGET
                    }),
                    self.fields.field(key::PORTS_TARGET).clone(),
                ))
                .child(ui::form_row(
                    tr(key::PORTS_TARGET_PORT),
                    self.fields.field(key::PORTS_TARGET_PORT).clone(),
                ));
        }
        if self.kind != ForwardKind::Dynamic {
            body = body.child(
                div().flex().gap_1().children(
                    [
                        (ForwardProtocol::Tcp, "TCP"),
                        (ForwardProtocol::Http, "HTTP"),
                        (ForwardProtocol::Https, "HTTPS"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (protocol, label))| {
                        div()
                            .id(("port-protocol", index))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .tab_index(0)
                            .rounded_sm()
                            .bg(rgb(if self.protocol == protocol {
                                ui::colors().selected
                            } else {
                                ui::colors().surface
                            }))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.protocol = protocol;
                                cx.emit(EditorEvent::Changed);
                                cx.notify();
                            }))
                    }),
                ),
            );
        }
        body.children(
            self.error
                .clone()
                .map(|error| div().text_color(rgb(ui::colors().danger)).child(error)),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    ui::text_button(
                        "port-editor-save",
                        key::PORTS_SAVE,
                        ui::ButtonStyle::Primary,
                    )
                    .on_click(cx.listener(
                        |this, _, _, cx| match this.value(cx) {
                            Ok((rule, automatic)) => cx.emit(EditorEvent::Save {
                                rule: Box::new(rule),
                                automatic,
                            }),
                            Err(error) => {
                                this.error = Some(error.to_string());
                                cx.notify();
                            }
                        },
                    )),
                )
                .child(
                    ui::text_button(
                        "port-editor-cancel",
                        key::COMMON_CANCEL,
                        ui::ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::Cancel))),
                ),
        )
    }
}
