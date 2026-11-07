//! One sampler per SSH session or Docker engine/container, shared by all views.
mod sampling;
mod status;
#[cfg(test)]
mod tests;
use crate::app::interaction::WindowState;
use gpui::{App, AppContext, Context, Entity, Task, WeakEntity};
use rustterm::{
    config::Uuid,
    docker::{DockerClient, Stats},
    session::{ResourceClient, ResourceSnapshot},
};
pub use status::MetricsView;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub(super) const TREND_POINTS: usize = 20;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(15);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceState {
    Loading,
    Live,
    Stale,
    Paused,
    Failed,
    Disconnected,
}
#[derive(Clone, Copy)]
pub(super) struct TrendPoint {
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub received: Option<f64>,
    pub sent: Option<f64>,
}
#[derive(Clone)]
enum Source {
    Host(ResourceClient),
    Container(DockerClient, String),
}
impl Source {
    fn connected(&self) -> bool {
        match self {
            Self::Host(client) => client.is_connected(),
            Self::Container(client, _) => client.is_connected(),
        }
    }
    fn reset_rates(&self) {
        if let Self::Host(client) = self {
            client.reset_rates();
        }
    }
    async fn release_idle(&self) {
        if let Self::Container(client, _) = self {
            client.release_idle().await;
        }
    }
}
#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Host(Uuid),
    Container(Uuid, String),
}
#[derive(Clone, Default)]
pub struct ResourceRegistry(Rc<RefCell<HashMap<Key, WeakEntity<ResourceMonitor>>>>);
impl ResourceRegistry {
    pub fn host(
        &self,
        id: Uuid,
        client: ResourceClient,
        window_state: WindowState,
        cx: &mut App,
    ) -> Entity<ResourceMonitor> {
        self.get(Key::Host(id), Source::Host(client), window_state, cx)
    }
    pub fn container(
        &self,
        client: DockerClient,
        id: String,
        window_state: WindowState,
        cx: &mut App,
    ) -> Entity<ResourceMonitor> {
        self.get(
            Key::Container(client.id(), id.clone()),
            Source::Container(client, id),
            window_state,
            cx,
        )
    }
    fn get(
        &self,
        key: Key,
        source: Source,
        window_state: WindowState,
        cx: &mut App,
    ) -> Entity<ResourceMonitor> {
        let mut entries = self.0.borrow_mut();
        if let Some(monitor) = entries.get(&key).and_then(WeakEntity::upgrade) {
            return monitor;
        }
        entries.retain(|_, entry| entry.upgrade().is_some());
        let monitor = cx.new(|cx| ResourceMonitor::new(source, window_state, cx));
        entries.insert(key, monitor.downgrade());
        monitor
    }
}
/// A view owns its interest. Dropping the view cannot leave an active subscriber.
pub struct MonitorInterest {
    value: Arc<AtomicU8>,
    wake: Arc<Notify>,
}
impl MonitorInterest {
    pub fn set(&self, active: bool, disks: bool) {
        let value = if active { 1 + u8::from(disks) } else { 0 };
        if self.value.swap(value, Ordering::AcqRel) != value {
            self.wake.notify_one();
        }
    }
}
impl Drop for MonitorInterest {
    fn drop(&mut self) {
        self.value.store(0, Ordering::Release);
        self.wake.notify_one();
    }
}
#[derive(Clone, Default)]
struct Interests {
    values: Arc<Mutex<Vec<Weak<AtomicU8>>>>,
    wake: Arc<Notify>,
}
impl Interests {
    fn subscribe(&self) -> MonitorInterest {
        let value = Arc::new(AtomicU8::new(0));
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(&value));
        MonitorInterest {
            value,
            wake: self.wake.clone(),
        }
    }
    fn demand(&self) -> u8 {
        let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        values.retain(|value| value.strong_count() > 0);
        values
            .iter()
            .filter_map(Weak::upgrade)
            .map(|value| value.load(Ordering::Acquire))
            .max()
            .unwrap_or(0)
    }
}
#[derive(Clone, Default)]
pub struct ResourceSummary {
    pub cpu: Option<f64>,
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
    pub rx: Option<f64>,
    pub tx: Option<f64>,
    pub network: Option<String>,
}
impl ResourceSummary {
    pub fn memory_percent(&self) -> Option<f64> {
        let (used, total) = (self.memory_used?, self.memory_total?);
        (total > 0).then(|| used as f64 * 100. / total as f64)
    }
}

pub struct ResourceMonitor {
    source: Source,
    window_state: WindowState,
    interests: Interests,
    pub(super) error: Option<String>,
    state: ResourceState,
    pub(super) snapshot: Option<ResourceSnapshot>,
    container: Option<Stats>,
    summary: ResourceSummary,
    sampled_at: Option<Instant>,
    pub(super) network: Option<String>,
    pub(super) trend: VecDeque<TrendPoint>,
    task: Option<Task<()>>,
}
impl ResourceMonitor {
    fn new(source: Source, window_state: WindowState, cx: &mut Context<Self>) -> Self {
        let mut monitor = Self {
            source,
            window_state,
            interests: Interests::default(),
            error: None,
            state: ResourceState::Paused,
            snapshot: None,
            container: None,
            summary: ResourceSummary::default(),
            sampled_at: None,
            network: None,
            trend: VecDeque::with_capacity(TREND_POINTS),
            task: None,
        };
        monitor.start_sampling(cx);
        monitor
    }
    pub fn subscribe(&self) -> MonitorInterest {
        self.interests.subscribe()
    }
    pub fn is_container(&self) -> bool {
        matches!(self.source, Source::Container(..))
    }
    pub fn state(&self) -> ResourceState {
        if !self.source.connected() {
            return ResourceState::Disconnected;
        }
        if self.interests.demand() == 0
            || self.window_state.is_locked()
            || !self.window_state.visibility.is_visible()
        {
            return ResourceState::Paused;
        }
        freshness(self.state, self.sampled_at, Instant::now())
    }
    pub fn summary(&self) -> Option<&ResourceSummary> {
        (self.state() == ResourceState::Live).then_some(&self.summary)
    }
    pub fn age(&self) -> Option<Duration> {
        self.sampled_at.map(|at| at.elapsed())
    }
    pub fn container_snapshot(&self) -> Option<&Stats> {
        (self.state() == ResourceState::Live)
            .then_some(self.container.as_ref())
            .flatten()
    }
    pub(super) fn current_snapshot(&self) -> Option<&ResourceSnapshot> {
        (self.state() == ResourceState::Live)
            .then_some(self.snapshot.as_ref())
            .flatten()
    }
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            return;
        }
        self.task = None;
        self.reset_rates();
        self.sampled_at = None;
        self.error = None;
        self.state = ResourceState::Loading;
        self.start_sampling(cx);
        cx.notify();
    }
    fn reset_rates(&mut self) {
        self.source.reset_rates();
        self.container = None;
        self.summary.cpu = None;
        self.summary.rx = None;
        self.summary.tx = None;
        if self.trend.back().is_some_and(|p| {
            p.cpu.is_some() || p.memory.is_some() || p.received.is_some() || p.sent.is_some()
        }) {
            self.push_trend(TrendPoint {
                cpu: None,
                memory: None,
                received: None,
                sent: None,
            });
        }
    }
    fn push_trend(&mut self, point: TrendPoint) {
        if self.trend.len() == TREND_POINTS {
            self.trend.pop_front();
        }
        self.trend.push_back(point);
    }
    fn set_state(&mut self, state: ResourceState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }
    fn accept_host(&mut self, snapshot: ResourceSnapshot, cx: &mut Context<Self>) {
        if self.network.as_ref().is_none_or(|name| {
            !snapshot
                .networks
                .iter()
                .any(|network| &network.name == name)
        }) {
            self.network = snapshot
                .networks
                .iter()
                .filter(|n| {
                    (n.name != "lo" || snapshot.networks.len() == 1)
                        && (n.rx_bytes_per_sec.is_some() || n.tx_bytes_per_sec.is_some())
                })
                .max_by(|a, b| {
                    let rate = |n: &rustterm::session::NetworkUsage| {
                        n.rx_bytes_per_sec.unwrap_or(0.) + n.tx_bytes_per_sec.unwrap_or(0.)
                    };
                    rate(a).total_cmp(&rate(b))
                })
                .map(|n| n.name.clone());
            for point in &mut self.trend {
                point.received = None;
                point.sent = None;
            }
        }
        let network = snapshot
            .networks
            .iter()
            .find(|n| Some(&n.name) == self.network.as_ref());
        self.summary = ResourceSummary {
            cpu: snapshot.cpu_percent,
            memory_used: Some(snapshot.memory.used_bytes),
            memory_total: Some(snapshot.memory.total_bytes),
            rx: network.and_then(|n| n.rx_bytes_per_sec),
            tx: network.and_then(|n| n.tx_bytes_per_sec),
            network: self.network.clone(),
        };
        self.sampled_at = Some(snapshot.sampled_at);
        self.snapshot = Some(snapshot);
        self.accept(cx);
    }
    fn accept_container(&mut self, stats: Stats, now: Instant, cx: &mut Context<Self>) {
        let previous = self.container.as_ref().zip(self.sampled_at);
        let (rx, tx) = previous.map_or((None, None), |(previous, at)| {
            container_rates(&stats, previous, now.saturating_duration_since(at))
        });
        self.summary = ResourceSummary {
            cpu: previous.and_then(|(previous, _)| stats.cpu_percent(previous)),
            memory_used: stats.memory_usage(),
            memory_total: stats.memory_stats.limit,
            rx,
            tx,
            network: None,
        };
        self.container = Some(stats);
        self.sampled_at = Some(now);
        self.accept(cx);
    }
    fn accept(&mut self, cx: &mut Context<Self>) {
        self.push_trend(TrendPoint {
            cpu: self.summary.cpu,
            memory: self.summary.memory_percent(),
            received: self.summary.rx,
            sent: self.summary.tx,
        });
        self.error = None;
        self.state = ResourceState::Live;
        cx.notify();
    }
    pub(super) fn select_network(&mut self, name: String, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.network.as_ref() == Some(&name) {
            return;
        }
        let Some(network) = self
            .current_snapshot()
            .and_then(|s| s.networks.iter().find(|n| n.name == name))
        else {
            return;
        };
        (self.summary.rx, self.summary.tx) = (network.rx_bytes_per_sec, network.tx_bytes_per_sec);
        self.summary.network = Some(name.clone());
        self.network = Some(name);
        for point in &mut self.trend {
            point.received = None;
            point.sent = None;
        }
        if let Some(point) = self.trend.back_mut() {
            (point.received, point.sent) = (self.summary.rx, self.summary.tx);
        }
        cx.notify();
    }
}
fn freshness(state: ResourceState, sampled_at: Option<Instant>, now: Instant) -> ResourceState {
    if state == ResourceState::Live
        && sampled_at.is_none_or(|at| now.saturating_duration_since(at) >= STALE_AFTER)
    {
        ResourceState::Stale
    } else {
        state
    }
}
fn container_rates(
    current: &Stats,
    previous: &Stats,
    elapsed: Duration,
) -> (Option<f64>, Option<f64>) {
    if elapsed.is_zero()
        || current.networks.is_empty()
        || current.networks.keys().ne(previous.networks.keys())
    {
        return (None, None);
    }
    let mut rx = 0u64;
    let mut tx = 0u64;
    for (name, current) in &current.networks {
        let previous = &previous.networks[name];
        let (Some(received), Some(sent)) = (
            current.rx_bytes.checked_sub(previous.rx_bytes),
            current.tx_bytes.checked_sub(previous.tx_bytes),
        ) else {
            return (None, None);
        };
        rx = rx.saturating_add(received);
        tx = tx.saturating_add(sent);
    }
    (
        Some(rx as f64 / elapsed.as_secs_f64()),
        Some(tx as f64 / elapsed.as_secs_f64()),
    )
}
pub(super) fn memory_percent(snapshot: &ResourceSnapshot) -> Option<f64> {
    (snapshot.memory.total_bytes > 0)
        .then(|| snapshot.memory.used_bytes as f64 * 100. / snapshot.memory.total_bytes as f64)
}
