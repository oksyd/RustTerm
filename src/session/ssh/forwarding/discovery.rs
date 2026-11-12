use super::*;
use crate::{config::ssh::ServiceIdentity, docker::DockerClient, session::exec::ExecClient};
use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Instant,
};
use tokio::sync::watch;

#[cfg(test)]
mod tests;

const LIMIT: usize = 1024 * 1024;
pub(super) const MAX_SERVICES: usize = 256;
const COMMAND: &str = include_str!("listeners.sh");

#[derive(Default)]
pub(super) struct OutputHints {
    tail: Vec<u8>,
    last: Option<Instant>,
}
impl OutputHints {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u16, crate::config::ssh::ForwardProtocol)> {
        if bytes.len() > 2048 {
            self.tail.clear();
        }
        self.tail
            .extend_from_slice(&bytes[bytes.len().saturating_sub(2048)..]);
        if self.tail.len() > 4096 {
            self.tail.drain(..self.tail.len() - 4096);
        }
        if self
            .last
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return Vec::new();
        }
        static URL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let pattern = URL.get_or_init(|| {
            regex::Regex::new(
                r"https?://(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]):([0-9]{1,5})[/\s\x1b]",
            )
            .unwrap()
        });
        let text = String::from_utf8_lossy(&self.tail);
        let hints = pattern
            .captures_iter(&text)
            .take(8)
            .filter_map(|capture| {
                let port = capture[1].parse::<u16>().ok().filter(|port| *port > 0)?;
                let protocol = if capture[0].starts_with("https:") {
                    crate::config::ssh::ForwardProtocol::Https
                } else {
                    crate::config::ssh::ForwardProtocol::Http
                };
                Some((port, protocol))
            })
            .collect::<Vec<_>>();
        if !hints.is_empty() {
            self.last = Some(Instant::now());
            self.tail.clear();
        }
        hints
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredService {
    pub identity: ServiceIdentity,
    pub name: String,
    pub project: Option<String>,
    pub host: String,
    pub port: u16,
    pub owned: bool,
    pub published: bool,
}
impl DiscoveredService {
    pub fn key(&self) -> String {
        self.identity.key()
    }
    pub(super) fn docker(&self) -> bool {
        !matches!(self.identity, ServiceIdentity::Process { .. })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryStatus {
    pub services: Vec<DiscoveredService>,
    pub scanning: bool,
    pub error: Option<String>,
    pub docker_error: Option<String>,
}

pub(super) struct Sample {
    pub host: Option<Vec<DiscoveredService>>,
    pub docker: Option<Vec<DiscoveredService>>,
    pub error: Option<String>,
    pub docker_error: Option<String>,
}

pub(super) async fn collect_host(
    exec: &ExecClient,
    ssh_port: u16,
) -> Result<Vec<DiscoveredService>, String> {
    let query = async {
        let stream = exec
            .open(format!("sh -c '{}'", COMMAND.replace('\'', "'\\''")))
            .await
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        stream
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        if bytes.len() > LIMIT {
            return Err(crate::i18n::tr(i18n_key::PORTS_LIMIT).into());
        }
        parse_host(
            std::str::from_utf8(&bytes).map_err(|_| "invalid UTF-8".to_owned())?,
            ssh_port,
        )
    };
    tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .map_err(|_| {
            crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_FORWARDING_REQUEST_TIMED_OUT).to_owned()
        })?
}

fn parse_address(text: &str) -> Option<(IpAddr, u16)> {
    let (host, port) = text.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    // Scoped link-local addresses need an interface context, unavailable across
    // the tunnel. Do not guess an interface or widen the listening scope.
    let ip = match host {
        "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        _ => host.parse().ok()?,
    };
    let port = port.parse::<u16>().ok().filter(|p| *p > 0)?;
    Some((ip, port))
}

fn target(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    }
}

pub(super) fn parse_host(text: &str, ssh_port: u16) -> Result<Vec<DiscoveredService>, String> {
    let mut lines = text.lines();
    let uid = lines
        .next()
        .and_then(|line| line.strip_prefix("RUSTTERM_PORTS_V1 "))
        .and_then(|uid| uid.parse::<u32>().ok())
        .ok_or_else(|| crate::i18n::tr(i18n_key::PORTS_LINUX_REQUIRED).to_owned())?;
    if !text.lines().any(|line| line == "RUSTTERM_PORTS_END") {
        return Err(crate::i18n::tr(i18n_key::PORTS_SCAN_FAILED).into());
    }
    let mut processes = HashMap::new();
    let mut actual_ssh_port = None;
    for line in text.lines() {
        if let Some(line) = line.strip_prefix("RUSTTERM_PROCESS\t") {
            let mut fields = line.splitn(3, '\t');
            if let (Some(pid), Some(owner), Some(executable)) =
                (fields.next(), fields.next(), fields.next())
                && let (Ok(pid), Ok(owner)) = (pid.parse::<u32>(), owner.parse::<u32>())
                && executable.starts_with('/')
                && executable.len() <= 512
                && !executable.chars().any(char::is_control)
            {
                processes.insert(pid, (owner, executable));
            }
        }
        if let Some(port) = line.strip_prefix("RUSTTERM_SSH_PORT ") {
            actual_ssh_port = port.parse::<u16>().ok();
        }
    }
    let mut services = BTreeMap::new();
    for line in lines.take_while(|line| *line != "RUSTTERM_PORTS_END") {
        let fields: Vec<_> = line.split_whitespace().take(6).collect();
        if fields.len() < 5 || fields[0] != "LISTEN" {
            continue;
        }
        let Some((ip, port)) = parse_address(fields[3]) else {
            continue;
        };
        if port == ssh_port || Some(port) == actual_ssh_port || ip.is_multicast() {
            continue;
        }
        let process = line
            .split("pid=")
            .skip(1)
            .filter_map(|tail| {
                let number: String = tail.chars().take_while(char::is_ascii_digit).collect();
                processes.get(&number.parse::<u32>().ok()?)
            })
            .find(|(owner, _)| *owner == uid);
        let executable = process.map(|(_, path)| *path).unwrap_or("unknown");
        let identity = ServiceIdentity::Process {
            executable: executable.to_owned(),
            address: ip,
            port,
        };
        let service = DiscoveredService {
            identity,
            name: executable
                .rsplit('/')
                .next()
                .unwrap_or(executable)
                .to_owned(),
            project: None,
            host: target(ip).to_string(),
            port,
            owned: process.is_some(),
            published: true,
        };
        services.insert(service.key(), service);
        if services.len() > MAX_SERVICES {
            return Err(crate::i18n::tr(i18n_key::PORTS_LIMIT).into());
        }
    }
    Ok(services.into_values().collect())
}

pub(super) fn docker_services(
    containers: Vec<crate::docker::Container>,
) -> Result<Vec<DiscoveredService>, String> {
    let mut services = BTreeMap::new();
    for container in containers.into_iter().filter(|c| c.running()) {
        for port in &container.ports {
            if port.kind != "tcp" || port.private_port == 0 {
                continue;
            }
            let project = container.project();
            let identity = match container.labels.get("com.docker.compose.service") {
                Some(service) if !project.is_empty() => ServiceIdentity::Compose {
                    project: project.to_owned(),
                    service: service.clone(),
                    replica: container
                        .labels
                        .get("com.docker.compose.container-number")
                        .cloned()
                        .unwrap_or_else(|| "1".into()),
                    port: port.private_port,
                },
                _ => ServiceIdentity::Container {
                    name: container.name().to_owned(),
                    port: port.private_port,
                },
            };
            if !identity.validate() {
                continue;
            }
            let Some(ip) = port
                .ip
                .as_deref()
                .unwrap_or("0.0.0.0")
                .parse::<IpAddr>()
                .ok()
            else {
                continue;
            };
            let published = port.public_port.is_some_and(|p| p > 0);
            let service = DiscoveredService {
                identity,
                name: container
                    .labels
                    .get("com.docker.compose.service")
                    .map(String::as_str)
                    .unwrap_or_else(|| container.name())
                    .chars()
                    .take(256)
                    .collect(),
                project: (!project.is_empty()).then(|| project.to_owned()),
                host: target(ip).to_string(),
                port: port.public_port.unwrap_or(port.private_port),
                owned: false,
                published,
            };
            // IPv4/IPv6 publications of the same service share one entry. Prefer
            // a reachable published mapping, then IPv4, independent of API order.
            let replace = services
                .get(&service.key())
                .is_none_or(|old: &DiscoveredService| {
                    (!old.published && service.published)
                        || (old.published == service.published
                            && old.host.contains(':')
                            && !service.host.contains(':'))
                });
            if replace {
                services.insert(service.key(), service);
            }
            if services.len() > MAX_SERVICES {
                return Err(crate::i18n::tr(i18n_key::PORTS_LIMIT).into());
            }
        }
    }
    Ok(services.into_values().collect())
}

pub(super) fn start(
    exec: ExecClient,
    ssh_port: u16,
    mut refresh: watch::Receiver<u64>,
    stop: CancellationToken,
) -> (mpsc::Receiver<Sample>, JoinHandle<()>) {
    let (send, receive) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        let docker = DockerClient::new(exec.clone());
        let mut events = None;
        let mut next_docker = Instant::now();
        let mut next_events = Instant::now();
        let mut unchanged = 0u8;
        let mut previous = Vec::new();
        loop {
            let sample = async {
                let host = collect_host(&exec, ssh_port).await;
                let (host, error) = match host {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error)),
                };
                let mut sample = Sample {
                    host,
                    docker: None,
                    error,
                    docker_error: None,
                };
                if Instant::now() >= next_docker {
                    match tokio::time::timeout(Duration::from_secs(5), docker.containers()).await {
                        Ok(Ok(containers)) => {
                            match docker_services(containers) {
                                Ok(services) => sample.docker = Some(services),
                                Err(error) => sample.docker_error = Some(error),
                            }
                            next_docker = Instant::now() + Duration::from_secs(30);
                            if events.is_none() && Instant::now() >= next_events {
                                events = tokio::time::timeout(
                                    Duration::from_secs(3),
                                    docker.container_events(),
                                )
                                .await
                                .ok()
                                .and_then(Result::ok);
                                next_events = Instant::now() + Duration::from_secs(30);
                            }
                        }
                        _ => {
                            sample.docker_error =
                                Some(crate::i18n::tr(i18n_key::PORTS_DOCKER_UNAVAILABLE).into());
                            next_docker = Instant::now() + Duration::from_secs(30);
                        }
                    }
                }
                sample
            };
            let sample =
                tokio::select! { _ = stop.cancelled() => break, sample = sample => sample };
            if let Some(host) = &sample.host {
                if previous == *host {
                    unchanged = unchanged.saturating_add(1);
                } else {
                    unchanged = 0;
                    previous = host.clone();
                }
            }
            let delay = if sample.error.is_some() {
                15
            } else if unchanged >= 10 {
                10
            } else {
                3
            };
            tokio::select! { _ = stop.cancelled() => break, result = send.send(sample) => if result.is_err() { break; } }
            let event = async {
                match &mut events {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(delay)) => {},
                result = refresh.changed() => { if result.is_err() { break; } refresh.borrow_and_update(); next_docker = Instant::now(); },
                update = event => {
                    if !matches!(update, Some(Ok(_))) { events = None; next_events = Instant::now() + Duration::from_secs(30); }
                    next_docker = Instant::now();
                    // Coalesce an event burst; a Compose restart can emit many
                    // records. Retain only a wake-up, never a history of events.
                    tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
                },
            }
        }
    });
    (receive, task)
}
