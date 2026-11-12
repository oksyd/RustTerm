use super::*;
use crate::config::ssh::{AutoForwardRule, ForwardProtocol};
use std::{collections::HashMap, net::Ipv4Addr, time::Instant};
use tokio::sync::watch;

#[cfg(test)]
mod tests;

const RESTART_GRACE: Duration = Duration::from_secs(10);
const MAX_RULES: usize = 288;

struct Observed {
    service: DiscoveredService,
    id: Uuid,
    seen: Instant,
    present: bool,
    missing_since: Option<Instant>,
}

struct Manager {
    settings: ForwardSettings,
    rules: Vec<PortForward>,
    runtime: ForwardRuntime,
    catalog: HashMap<String, Observed>,
    baseline: bool,
    refresh_requested: bool,
    refresh_sent: bool,
    hints: HashMap<u16, (ForwardProtocol, Instant)>,
}

impl Manager {
    fn new(settings: ForwardSettings) -> Self {
        let mut manager = Self {
            rules: settings.rules.clone(),
            runtime: ForwardRuntime::new(&settings.rules),
            settings,
            catalog: HashMap::new(),
            baseline: false,
            refresh_requested: false,
            refresh_sent: false,
            hints: HashMap::new(),
        };
        manager.seed_policies();
        manager
    }

    fn seed_policies(&mut self) {
        let saved: std::collections::HashSet<_> = self
            .settings
            .automatic
            .services
            .iter()
            .map(|p| p.service.key())
            .collect();
        for policy in &self.settings.automatic.services {
            let identity = &policy.service;
            if !self.catalog.contains_key(&identity.key())
                && self.catalog.len() >= discovery::MAX_SERVICES
                && let Some(key) = self
                    .catalog
                    .iter()
                    .filter(|(key, _)| !saved.contains(*key))
                    .min_by_key(|(_, value)| value.seen)
                    .map(|(key, _)| key.clone())
            {
                self.catalog.remove(&key);
            }
            let (name, project, host, port) = match identity {
                crate::config::ssh::ServiceIdentity::Process {
                    executable,
                    address,
                    port,
                } => (
                    executable
                        .rsplit('/')
                        .next()
                        .unwrap_or(executable)
                        .to_owned(),
                    None,
                    if address.is_unspecified() {
                        if address.is_ipv4() {
                            "127.0.0.1".into()
                        } else {
                            "::1".into()
                        }
                    } else {
                        address.to_string()
                    },
                    *port,
                ),
                crate::config::ssh::ServiceIdentity::Compose {
                    project,
                    service,
                    port,
                    ..
                } => (
                    service.clone(),
                    Some(project.clone()),
                    "127.0.0.1".into(),
                    *port,
                ),
                crate::config::ssh::ServiceIdentity::Container { name, port } => {
                    (name.clone(), None, "127.0.0.1".into(), *port)
                }
            };
            self.catalog
                .entry(identity.key())
                .or_insert_with(|| Observed {
                    service: DiscoveredService {
                        identity: identity.clone(),
                        name,
                        project,
                        host,
                        port,
                        owned: false,
                        published: false,
                    },
                    id: Uuid::new_v4(),
                    seen: Instant::now(),
                    present: false,
                    missing_since: Some(Instant::now() - RESTART_GRACE),
                });
        }
    }

    fn publish_settings(&self, controller: &ForwardController) {
        controller
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current = self.settings.clone();
    }

    fn apply_sample(&mut self, sample: discovery::Sample, controller: &ForwardController) {
        let now = Instant::now();
        let mut limited = false;
        let first_host = !self.baseline;
        let docker_checked = sample.docker.is_some();
        let docker_ports: Vec<_> = sample
            .docker
            .as_ref()
            .map(|services| {
                services
                    .iter()
                    .filter(|s| s.published)
                    .map(|s| (s.host.clone(), s.port))
                    .collect()
            })
            .unwrap_or_else(|| {
                self.catalog
                    .values()
                    .filter(|o| o.present && o.service.docker() && o.service.published)
                    .map(|o| (o.service.host.clone(), o.service.port))
                    .collect()
            });
        if sample.host.is_some() {
            self.baseline = true;
        }
        for (docker, services) in [(false, sample.host), (true, sample.docker)] {
            let Some(services) = services else {
                continue;
            };
            let previously_present: std::collections::HashSet<_> = self
                .catalog
                .iter()
                .filter(|(_, o)| o.present && o.service.docker() == docker)
                .map(|(key, _)| key.clone())
                .collect();
            for observed in self
                .catalog
                .values_mut()
                .filter(|o| o.service.docker() == docker)
            {
                if observed.present {
                    observed.missing_since.get_or_insert(now);
                }
                observed.present = false;
            }
            for service in services {
                if !docker && docker_ports.contains(&(service.host.clone(), service.port)) {
                    continue;
                }
                let key = service.key();
                let new = !self.catalog.contains_key(&key);
                let selected_project = service
                    .project
                    .as_ref()
                    .is_some_and(|p| self.settings.automatic.projects.contains(p));
                let hint = self
                    .hints
                    .get(&service.port)
                    .filter(|(_, at)| at.elapsed() < Duration::from_secs(30));
                let auto = self.settings.automatic.enabled
                    && service.published
                    && ((!previously_present.contains(&key)
                        && !first_host
                        && service.owned
                        && !docker)
                        || selected_project
                        || (hint.is_some() && (service.owned || docker)));
                let enroll = auto
                    && !self
                        .settings
                        .automatic
                        .services
                        .iter()
                        .any(|r| r.service == service.identity);
                limited |= enroll && self.settings.automatic.services.len() >= 128;
                if enroll && self.settings.automatic.services.len() < 128 {
                    self.settings.automatic.services.push(AutoForwardRule {
                        service: service.identity.clone(),
                        enabled: true,
                        bind_address: Ipv4Addr::LOCALHOST.into(),
                        local_port: None,
                        last_port: None,
                        protocol: hint.map_or(ForwardProtocol::Tcp, |(protocol, _)| *protocol),
                    });
                }
                if new && self.catalog.len() >= discovery::MAX_SERVICES {
                    // Bound metadata even if a server continually changes ports.
                    if let Some(oldest) = self
                        .catalog
                        .iter()
                        .filter(|(_, o)| {
                            !o.present
                                && !self
                                    .settings
                                    .automatic
                                    .services
                                    .iter()
                                    .any(|p| p.service == o.service.identity)
                        })
                        .min_by_key(|(_, o)| o.seen)
                        .map(|(k, _)| k.clone())
                    {
                        self.catalog.remove(&oldest);
                    } else {
                        limited = true;
                        continue;
                    }
                }
                let observed = self.catalog.entry(key).or_insert_with(|| Observed {
                    service: service.clone(),
                    id: Uuid::new_v4(),
                    seen: now,
                    present: true,
                    missing_since: None,
                });
                observed.service = service;
                observed.present = true;
                observed.missing_since = None;
                observed.seen = now;
            }
        }
        let mut state = controller
            .discovery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.scanning = false;
        state.error = sample
            .error
            .or_else(|| limited.then(|| crate::i18n::tr(i18n_key::PORTS_LIMIT).to_owned()));
        // A skipped Docker calibration must not erase a previous failure.
        if sample.docker_error.is_some() {
            state.docker_error = sample.docker_error;
        } else if docker_checked {
            state.docker_error = None;
        }
        state.services = self
            .catalog
            .values()
            .filter(|o| o.present)
            .map(|o| o.service.clone())
            .collect();
        state.services.sort_by(|a, b| {
            (&a.project, &a.name, a.port, &a.host).cmp(&(&b.project, &b.name, b.port, &b.host))
        });
        self.publish_settings(controller);
    }

    fn desired(&self) -> Vec<ForwardStatus> {
        let mut desired: Vec<_> = self
            .settings
            .rules
            .iter()
            .cloned()
            .map(ForwardStatus::new)
            .collect();
        for observed in self.catalog.values() {
            let service = &observed.service;
            let Some(policy) = self
                .settings
                .automatic
                .services
                .iter()
                .find(|r| r.service == service.identity)
            else {
                continue;
            };
            if self.settings.rules.iter().any(|r| {
                r.kind == ForwardKind::Local
                    && r.destination_host == service.host
                    && r.destination_port == service.port
            }) {
                continue;
            }
            let within_grace = observed.present
                || observed
                    .missing_since
                    .is_some_and(|at| at.elapsed() < RESTART_GRACE);
            let rule = PortForward {
                id: observed.id,
                name: service.name.clone(),
                protocol: policy.protocol,
                kind: ForwardKind::Local,
                bind_host: policy.bind_address.to_string(),
                bind_port: policy
                    .local_port
                    .or(policy.last_port)
                    .unwrap_or(service.port),
                destination_host: service.host.clone(),
                destination_port: service.port,
                enabled: self.settings.automatic.enabled
                    && policy.enabled
                    && service.published
                    && within_grace,
            };
            let mut status = ForwardStatus::new(rule);
            status.enabled = self.settings.automatic.enabled && policy.enabled;
            status.service = Some(service.identity.clone());
            status.available = observed.present && service.published;
            status.port_fallback = policy.local_port.is_none();
            desired.push(status);
        }
        desired
    }

    fn enable(&mut self, id: Uuid, enabled: bool) {
        if let Some(rule) = self.settings.rules.iter_mut().find(|r| r.id == id) {
            rule.enabled = enabled;
        }
        if let Some(observed) = self.catalog.values().find(|o| o.id == id)
            && let Some(policy) = self
                .settings
                .automatic
                .services
                .iter_mut()
                .find(|r| r.service == observed.service.identity)
        {
            policy.enabled = enabled;
        }
    }

    fn control(
        &mut self,
        event: Control,
        options: &mut SshOptions,
        controller: &ForwardController,
    ) -> bool {
        let mut retry = None;
        let mut acknowledged = None;
        match event {
            Control::Configure(settings, revision) => {
                if settings.validate().is_err() {
                    return false;
                }
                let previous = self.desired();
                acknowledged = Some(revision);
                if settings.automatic.enabled && !self.settings.automatic.enabled {
                    self.baseline = false;
                }
                self.settings = settings;
                self.seed_policies();
                for next in self.desired() {
                    if previous
                        .iter()
                        .find(|old| old.rule.id == next.rule.id)
                        .is_none_or(|old| old.rule != next.rule)
                        && let Some(index) =
                            self.rules.iter().position(|rule| rule.id == next.rule.id)
                        && matches!(self.runtime.rules[index].phase, RulePhase::Failed)
                    {
                        self.runtime.rules[index].phase = RulePhase::Stopped;
                    }
                }
            }
            Control::RuleEnabled(id, enabled) => {
                self.enable(id, enabled);
                if enabled {
                    retry = Some(id);
                }
            }
            Control::RuleRetry(id) => retry = Some(id),
            Control::SetEnabled(index, enabled) => {
                if let Some(rule) = self.rules.get(index) {
                    let id = rule.id;
                    self.enable(id, enabled);
                    if enabled {
                        retry = Some(id);
                    }
                }
            }
            Control::Retry(index) => retry = self.rules.get(index).map(|r| r.id),
            Control::Refresh => {
                self.refresh_requested = true;
                self.refresh_sent = false;
            }
            Control::Hint(port, protocol) => {
                if self.settings.automatic.enabled {
                    self.hints
                        .retain(|_, (_, at)| at.elapsed() < Duration::from_secs(30));
                    if self.hints.len() < 32 {
                        self.hints.insert(port, (protocol, Instant::now()));
                    }
                    self.refresh_requested = true;
                    self.refresh_sent = false;
                }
            }
            Control::Credentials(fresh) => {
                options.auth = fresh.auth.clone();
                options.authentication = fresh.authentication.clone();
                options.keepalive_secs = fresh.keepalive_secs;
                options.keepalive_max = fresh.keepalive_max;
            }
        }
        if let Some(id) = retry
            && let Some(index) = self.rules.iter().position(|r| r.id == id)
            && matches!(self.runtime.rules[index].phase, RulePhase::Failed)
        {
            self.runtime.rules[index].phase = RulePhase::Stopped;
        }
        self.publish_settings(controller);
        if let Some(revision) = acknowledged {
            let mut store = controller
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if store
                .pending
                .as_ref()
                .is_some_and(|(pending, _)| *pending == revision)
            {
                store.pending = None;
            }
        }
        self.settings.automatic.enabled
            || self.refresh_requested
            || self.settings.rules.iter().any(|r| r.enabled)
    }

    fn reconcile(
        &mut self,
        controller: &ForwardController,
        connection: Option<&ForwardConnection>,
    ) {
        self.remember_assignments(controller);
        let desired = self.desired();
        for index in 0..self.rules.len() {
            let next = desired.iter().find(|s| s.rule.id == self.rules[index].id);
            let same = next.is_some_and(|next| same_endpoint(&next.rule, &self.rules[index]));
            let wanted = next.is_some_and(|s| s.rule.enabled) && same;
            self.runtime.rules[index].desired = wanted;
            if !wanted
                && let Some(connection) = connection
                && matches!(
                    self.runtime.rules[index].phase,
                    RulePhase::Starting(_) | RulePhase::Running(_)
                )
            {
                disable(
                    index,
                    &self.rules[index],
                    controller,
                    connection,
                    &mut self.runtime,
                );
            }
            if matches!(
                self.runtime.rules[index].phase,
                RulePhase::Stopped | RulePhase::Failed
            ) {
                if let Some(next) = next {
                    if !same {
                        self.runtime.rules[index].phase = RulePhase::Stopped;
                        self.rules[index] = next.rule.clone();
                        controller.states.lock().unwrap_or_else(|e| e.into_inner())[index] =
                            next.clone();
                        self.runtime.rules[index].desired = next.rule.enabled;
                    }
                } else {
                    self.rules[index].id = Uuid::nil();
                    controller.states.lock().unwrap_or_else(|e| e.into_inner())[index]
                        .rule
                        .id = Uuid::nil();
                    self.runtime.rules[index].phase = RulePhase::Stopped;
                }
            }
        }
        for next in desired {
            let index = if let Some(index) = self.rules.iter().position(|r| r.id == next.rule.id) {
                index
            } else if let Some(index) = self.rules.iter().position(|r| r.id.is_nil()) {
                self.rules[index] = next.rule.clone();
                self.runtime.rules[index].generation =
                    self.runtime.rules[index].generation.wrapping_add(1);
                self.runtime.rules[index].desired = next.rule.enabled;
                controller.states.lock().unwrap_or_else(|e| e.into_inner())[index] = next.clone();
                index
            } else {
                if self.rules.len() >= MAX_RULES {
                    continue;
                }
                self.rules.push(next.rule.clone());
                self.runtime.rules.push(RuleRuntime {
                    desired: next.rule.enabled,
                    generation: 0,
                    phase: RulePhase::Stopped,
                });
                controller
                    .states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(next.clone());
                self.rules.len() - 1
            };
            {
                let mut statuses = controller.states.lock().unwrap_or_else(|e| e.into_inner());
                let status = &mut statuses[index];
                status.enabled = next.enabled;
                status.available = next.available;
                status.service = next.service;
                status.port_fallback = next.port_fallback;
                status.rule.name = next.rule.name.clone();
                status.rule.protocol = next.rule.protocol;
                status.rule.enabled = next.rule.enabled;
                if !next.available && next.enabled {
                    status.state = ForwardState::Waiting;
                } else if matches!(self.runtime.rules[index].phase, RulePhase::Running(_)) {
                    status.state = ForwardState::Running;
                } else if !next.enabled
                    && matches!(self.runtime.rules[index].phase, RulePhase::Stopped)
                {
                    status.state = ForwardState::Stopped;
                }
            }
            if self.runtime.rules[index].desired
                && matches!(self.runtime.rules[index].phase, RulePhase::Stopped)
                && let Some(connection) = connection
            {
                activate(
                    index,
                    &self.rules[index],
                    controller,
                    connection,
                    &mut self.runtime,
                );
            }
        }
        self.publish_settings(controller);
    }

    fn remember_assignments(&mut self, controller: &ForwardController) {
        // Remember automatic assignments only after a successful bind; this
        // preserves phone URLs across application restarts without a bind race.
        for status in controller.statuses() {
            if let (Some(identity), Some(address)) = (&status.service, status.local_address)
                && let Some(policy) = self
                    .settings
                    .automatic
                    .services
                    .iter_mut()
                    .find(|r| &r.service == identity)
            {
                policy.last_port = Some(address.port());
                if policy.local_port.is_none()
                    && policy.bind_address.to_string() == status.rule.bind_host
                    && let Some(rule) = self.rules.iter_mut().find(|r| r.id == status.rule.id)
                {
                    rule.bind_port = address.port();
                    if let Some(status) = controller
                        .states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter_mut()
                        .find(|s| s.rule.id == rule.id)
                    {
                        status.rule.bind_port = address.port();
                    }
                }
            }
        }
    }
    fn needs_connection(&self) -> bool {
        self.runtime.enabled() || self.settings.automatic.enabled || self.refresh_requested
    }
}

fn same_endpoint(a: &PortForward, b: &PortForward) -> bool {
    (
        a.kind,
        &a.bind_host,
        a.bind_port,
        &a.destination_host,
        a.destination_port,
    ) == (
        b.kind,
        &b.bind_host,
        b.bind_port,
        &b.destination_host,
        b.destination_port,
    )
}

pub(super) fn start(
    mut options: SshOptions,
    stop: CancellationToken,
) -> (ForwardController, JoinHandle<()>) {
    let settings = ForwardSettings {
        rules: std::mem::take(&mut options.forwards),
        automatic: std::mem::take(&mut options.auto_forward),
    };
    let (control, controls) = mpsc::channel(32);
    let controller = ForwardController {
        control,
        states: Arc::new(Mutex::new(
            settings
                .rules
                .iter()
                .cloned()
                .map(ForwardStatus::new)
                .collect(),
        )),
        remote: Arc::new(Mutex::new(BTreeMap::new())),
        slots: Arc::new(Semaphore::new(64)),
        settings: Arc::new(Mutex::new(SettingsStore::new(settings.clone()))),
        discovery: Arc::new(Mutex::new(DiscoveryStatus::default())),
        hints: Arc::new(Mutex::new(discovery::OutputHints::default())),
    };
    let active = controller.clone();
    let task = tokio::spawn(run(options, stop, active, controls, Manager::new(settings)));
    (controller, task)
}

async fn run(
    mut options: SshOptions,
    stop: CancellationToken,
    active: ForwardController,
    mut controls: mpsc::Receiver<Control>,
    mut manager: Manager,
) {
    let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let mut connect_requested = manager.needs_connection();
    let mut retry_at = None;
    let mut backoff = 1u64;
    loop {
        if stop.is_cancelled() {
            break;
        }
        manager.reconcile(&active, None);
        if !connect_requested || !manager.needs_connection() {
            let event = tokio::select! {
                _ = stop.cancelled() => break,
                event = controls.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            connect_requested |= manager.control(event, &mut options, &active);
            continue;
        }
        if let Some(at) = retry_at {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep_until(at) => {},
                event = controls.recv() => { if let Some(event) = event { manager.control(event, &mut options, &active); } else { break; } continue; },
            }
            retry_at = None;
        }
        connect_requested = false;
        let connection_stop = stop.child_token();
        let closed = CancellationToken::new();
        for (index, rule) in manager.runtime.rules.iter().enumerate() {
            if rule.desired {
                set_state(&active, index, ForwardState::Starting);
            }
        }
        let handle = {
            let connecting = connect_authenticated(
                options.clone(),
                Ui::Forwarding {
                    controller: active.clone(),
                    closed: closed.clone(),
                },
                connection_stop.clone(),
            );
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break None,
                    event = controls.recv() => {
                        let Some(event) = event else { break None; };
                        manager.control(event, &mut options, &active);
                        manager.reconcile(&active, None);
                        if !manager.needs_connection() { break None; }
                    }
                    result = &mut connecting => match result {
                        Ok(handle) => break Some(handle),
                        Err((kind, error)) => {
                            for (index, rule) in manager.runtime.rules.iter_mut().enumerate() {
                                if rule.desired { rule.phase = RulePhase::Failed; set_state(&active, index, ForwardState::Failed(error.clone())); }
                            }
                            active.discovery.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error);
                            // Authentication/trust rejection needs an explicit
                            // retry; never repeat an MFA prompt in a timer loop.
                            if matches!(kind, crate::session::FailureKind::Network | crate::session::FailureKind::Dns | crate::session::FailureKind::Timeout | crate::session::FailureKind::RemoteClosed) {
                                connect_requested = true;
                                retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(backoff));
                                backoff = (backoff * 2).min(30);
                            }
                            break None;
                        }
                    }
                }
            }
        };
        let Some(handle) = handle else {
            connection_stop.cancel();
            continue;
        };
        backoff = 1;
        let connection = ForwardConnection::new(handle, connection_stop, closed, timeout);
        let (refresh, refresh_rx) = watch::channel(0u64);
        let mut scanner = None::<(
            CancellationToken,
            mpsc::Receiver<discovery::Sample>,
            JoinHandle<()>,
        )>;
        // A new transport can retry failed rules; remote reservations belong to
        // the old transport and were released when that connection closed.
        for rule in &mut manager.runtime.rules {
            rule.phase = RulePhase::Stopped;
        }
        let failure = loop {
            manager.reconcile(&active, Some(&connection));
            let scanning = manager.settings.automatic.enabled || manager.refresh_requested;
            if scanning && scanner.is_none() {
                let cancel = connection.stop.child_token();
                let (samples, task) = discovery::start(
                    connection.exec.clone(),
                    options.port,
                    refresh_rx.clone(),
                    cancel.clone(),
                );
                scanner = Some((cancel, samples, task));
                active
                    .discovery
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .scanning = true;
            } else if !scanning && let Some((cancel, _, task)) = scanner.take() {
                cancel.cancel();
                task.abort();
            }
            if manager.refresh_requested && !manager.refresh_sent && scanner.is_some() {
                refresh.send_modify(|revision| *revision = revision.wrapping_add(1));
                manager.refresh_sent = true;
            }
            if !manager.needs_connection() {
                break None;
            }
            let next_sample = async {
                match &mut scanner {
                    Some((_, receive, _)) => receive.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = stop.cancelled() => break None,
                _ = connection.closed.cancelled() => { connect_requested = true; break Some(crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_FORWARDING_CONNECTION_CLOSED).to_owned()); },
                event = controls.recv() => {
                    let Some(event) = event else { break None; };
                    manager.control(event, &mut options, &active);
                },
                sample = next_sample => if let Some(sample) = sample { manager.apply_sample(sample, &active); manager.refresh_requested = false; manager.refresh_sent = false; } else { connect_requested = true; break Some(crate::i18n::tr(i18n_key::PORTS_SCAN_FAILED).into()); },
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                result = manager.runtime.activations.join_next(), if !manager.runtime.activations.is_empty() => {
                    match result {
                        Some(Ok((index, generation, result))) => manager.runtime.finish_start(index, generation, result, &manager.rules[index], &active, &connection),
                        _ => { connect_requested = true; break Some(crate::i18n::tr(i18n_key::PORTS_SCAN_FAILED).into()); }
                    }
                },
                result = manager.runtime.deactivations.join_next(), if !manager.runtime.deactivations.is_empty() => {
                    match result {
                        Some(Ok((index, generation, result))) => {
                            if let Err(error) = &result { connect_requested = true; break Some(error.clone()); }
                            manager.runtime.finish_stop(index, generation, result, &manager.rules[index], &active, &connection);
                        }
                        _ => { connect_requested = true; break Some(crate::i18n::tr(i18n_key::PORTS_SCAN_FAILED).into()); }
                    }
                },
                result = manager.runtime.listeners.join_next(), if !manager.runtime.listeners.is_empty() => {
                    match result {
                        Some(Ok((index, generation, error))) => manager.runtime.finish_listener(index, generation, error, &manager.rules[index], &active, &connection),
                        _ => { connect_requested = true; break Some(crate::i18n::tr(i18n_key::PORTS_SCAN_FAILED).into()); }
                    }
                },
            }
        };
        if let Some((cancel, _, task)) = scanner.take() {
            cancel.cancel();
            task.abort();
        }
        connection.stop.cancel();
        manager.runtime.stop_all(&active).await;
        connection.close().await;
        for (index, rule) in manager.runtime.rules.iter().enumerate() {
            set_state(
                &active,
                index,
                match (&failure, rule.desired) {
                    (Some(error), true) => ForwardState::Failed(error.clone()),
                    _ => ForwardState::Stopped,
                },
            );
        }
        if connect_requested {
            retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        }
    }
    manager.runtime.stop_all(&active).await;
    for index in 0..manager.rules.len() {
        set_state(&active, index, ForwardState::Stopped);
    }
}
