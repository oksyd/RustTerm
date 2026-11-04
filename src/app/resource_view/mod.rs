mod render;
mod sampling;

use crate::app::interaction::WindowState;
use gpui::{App, Context, EventEmitter, FocusHandle, Focusable, Task, Window};
use rustterm::{
    config::Uuid,
    session::{ResourceClient, ResourceSnapshot},
};
use std::collections::VecDeque;

const TREND_POINTS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceState {
    Loading,
    Live,
    Paused,
    Failed,
    Disconnected,
}

#[derive(Clone, Copy, Debug)]
pub enum ResourceEvent {
    Close,
}

#[derive(Clone, Copy)]
struct TrendPoint {
    cpu: Option<f64>,
    memory: Option<f64>,
    received: Option<f64>,
    sent: Option<f64>,
}

pub struct ResourceView {
    session_id: Uuid,
    client: ResourceClient,
    window_state: WindowState,
    focus: FocusHandle,
    active: bool,
    closed: bool,
    state: ResourceState,
    error: Option<String>,
    snapshot: Option<ResourceSnapshot>,
    network: Option<String>,
    trend: VecDeque<TrendPoint>,
    task: Option<Task<()>>,
}

impl EventEmitter<ResourceEvent> for ResourceView {}
impl Focusable for ResourceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ResourceView {
    pub fn new(
        session_id: Uuid,
        client: ResourceClient,
        window_state: WindowState,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session_id,
            client,
            window_state,
            focus: cx.focus_handle(),
            active: false,
            closed: false,
            state: ResourceState::Loading,
            error: None,
            snapshot: None,
            network: None,
            trend: VecDeque::with_capacity(TREND_POINTS),
            task: None,
        }
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.closed || self.active == active {
            return;
        }
        self.active = active;
        self.task = None;
        self.client.reset_rates();
        if active {
            self.start_sampling(cx);
        } else {
            self.break_trend();
            self.set_state(ResourceState::Paused, cx);
        }
    }

    fn break_trend(&mut self) {
        if self.trend.back().is_some_and(|point| {
            point.cpu.is_some()
                || point.memory.is_some()
                || point.received.is_some()
                || point.sent.is_some()
        }) {
            if self.trend.len() == TREND_POINTS {
                self.trend.pop_front();
            }
            self.trend.push_back(TrendPoint {
                cpu: None,
                memory: None,
                received: None,
                sent: None,
            });
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.set_active(false, cx);
        self.closed = true;
        self.task = None;
    }

    fn current_snapshot(&self) -> Option<&ResourceSnapshot> {
        (self.state == ResourceState::Live)
            .then_some(self.snapshot.as_ref())
            .flatten()
    }

    fn set_state(&mut self, state: ResourceState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.closed || self.window_state.is_locked() {
            return;
        }
        self.task = None;
        self.client.reset_rates();
        self.trend.clear();
        self.start_sampling(cx);
    }

    fn accept_sample(&mut self, snapshot: ResourceSnapshot, cx: &mut Context<Self>) {
        if self.network.as_ref().is_none_or(|name| {
            !snapshot
                .networks
                .iter()
                .any(|network| &network.name == name)
        }) {
            self.network = snapshot
                .networks
                .iter()
                .filter(|network| {
                    (network.name != "lo" || snapshot.networks.len() == 1)
                        && (network.rx_bytes_per_sec.is_some()
                            || network.tx_bytes_per_sec.is_some())
                })
                .max_by(|a, b| {
                    let rate = |network: &rustterm::session::NetworkUsage| {
                        network.rx_bytes_per_sec.unwrap_or(0.)
                            + network.tx_bytes_per_sec.unwrap_or(0.)
                    };
                    rate(a).total_cmp(&rate(b))
                })
                .map(|network| network.name.clone());
            for point in &mut self.trend {
                point.received = None;
                point.sent = None;
            }
        }
        let network = snapshot
            .networks
            .iter()
            .find(|network| Some(&network.name) == self.network.as_ref());
        if self.trend.len() == TREND_POINTS {
            self.trend.pop_front();
        }
        self.trend.push_back(TrendPoint {
            cpu: snapshot.cpu_percent,
            memory: memory_percent(&snapshot),
            received: network.and_then(|network| network.rx_bytes_per_sec),
            sent: network.and_then(|network| network.tx_bytes_per_sec),
        });
        self.snapshot = Some(snapshot);
        self.error = None;
        self.state = ResourceState::Live;
        cx.notify();
    }

    fn select_network(&mut self, name: String, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.network.as_ref() == Some(&name) {
            return;
        }
        let Some(network) = self.current_snapshot().and_then(|snapshot| {
            snapshot
                .networks
                .iter()
                .find(|network| network.name == name)
        }) else {
            return;
        };
        let rates = (network.rx_bytes_per_sec, network.tx_bytes_per_sec);
        self.network = Some(name);
        for point in &mut self.trend {
            point.received = None;
            point.sent = None;
        }
        if let Some(point) = self.trend.back_mut() {
            (point.received, point.sent) = rates;
        }
        cx.notify();
    }
}

fn memory_percent(snapshot: &ResourceSnapshot) -> Option<f64> {
    let memory = &snapshot.memory;
    (memory.total_bytes > 0).then(|| memory.used_bytes as f64 * 100. / memory.total_bytes as f64)
}
