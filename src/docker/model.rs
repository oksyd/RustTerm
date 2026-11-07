use super::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
#[derive(Clone, Debug, Deserialize)]
pub struct EngineVersion {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "ApiVersion")]
    pub api: String,
    #[serde(rename = "MinAPIVersion", default)]
    pub minimum_api: String,
    #[serde(rename = "Os", default)]
    pub os: String,
}
impl EngineVersion {
    pub(super) fn negotiate(&mut self) -> Result<()> {
        let maximum = api(&self.api)?;
        let minimum = if self.minimum_api.is_empty() {
            (1, 24)
        } else {
            api(&self.minimum_api)?
        };
        let selected = maximum.min((1, 47));
        if selected < (1, 41) || selected < minimum || minimum > maximum {
            return Err(Error::Version);
        }
        self.api = format!("{}.{}", selected.0, selected.1);
        Ok(())
    }
}
fn api(value: &str) -> Result<(u16, u16)> {
    let (major, minor) = value.split_once('.').ok_or(Error::Version)?;
    Ok((
        major.parse().map_err(|_| Error::Version)?,
        minor.parse().map_err(|_| Error::Version)?,
    ))
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Container {
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub names: Vec<String>,
    pub image: String,
    pub state: String,
    pub status: String,
    #[serde(default, deserialize_with = "null_default")]
    pub ports: Vec<Port>,
    #[serde(default, deserialize_with = "null_default")]
    pub labels: BTreeMap<String, String>,
}
impl Container {
    pub fn name(&self) -> &str {
        self.names
            .first()
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(&self.id)
    }
    pub fn project(&self) -> &str {
        self.labels
            .get("com.docker.compose.project")
            .map(String::as_str)
            .unwrap_or("")
    }
    pub fn running(&self) -> bool {
        self.state == "running"
    }
    pub fn ports(&self) -> String {
        self.ports
            .iter()
            .map(|port| match port.public_port {
                Some(public) => format!(
                    "{}:{public} → {}/{}",
                    port.ip.as_deref().unwrap_or("*"),
                    port.private_port,
                    port.kind
                ),
                None => format!("{}/{}", port.private_port, port.kind),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    pub fn matches(&self, search: &str) -> bool {
        let search = search.to_lowercase();
        [
            self.name(),
            &self.id,
            &self.image,
            self.project(),
            &self.status,
        ]
        .iter()
        .any(|s| s.to_lowercase().contains(&search))
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Port {
    #[serde(rename = "IP")]
    pub ip: Option<String>,
    pub private_port: u16,
    pub public_port: Option<u16>,
    #[serde(rename = "Type")]
    pub kind: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerDetails {
    pub id: String,
    pub name: String,
    pub created: String,
    pub config: ContainerConfig,
    pub state: ContainerState,
    #[serde(default, deserialize_with = "null_default")]
    pub mounts: Vec<Mount>,
    pub network_settings: NetworkSettings,
    pub host_config: HostConfig,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    pub image: String,
    pub tty: bool,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub working_dir: String,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub labels: Option<BTreeMap<String, String>>,
    // Environment values may contain credentials; do not retain or render them.
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerState {
    pub status: String,
    pub running: bool,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub error: String,
    pub health: Option<Health>,
    pub started_at: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Health {
    pub status: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Mount {
    #[serde(rename = "Type")]
    pub kind: String,
    pub source: String,
    pub destination: String,
    #[serde(rename = "RW")]
    pub writable: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfig {
    pub restart_policy: RestartPolicy,
    pub network_mode: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RestartPolicy {
    pub name: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkSettings {
    pub networks: Option<BTreeMap<String, Network>>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Network {
    #[serde(rename = "IPAddress")]
    pub address: String,
    #[serde(rename = "GlobalIPv6Address", default)]
    pub ipv6: String,
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct Stats {
    #[serde(default)]
    pub cpu_stats: CpuStats,
    #[serde(default)]
    pub memory_stats: MemoryStats,
    #[serde(default, deserialize_with = "null_default")]
    pub networks: BTreeMap<String, NetworkStats>,
    #[serde(default)]
    pub pids_stats: PidStats,
}
impl Stats {
    /// Linux CPU percentage, relative to one CPU, using consecutive one-shot samples.
    pub fn cpu_percent(&self, previous: &Self) -> Option<f64> {
        let cpu = self
            .cpu_stats
            .cpu_usage
            .total_usage
            .checked_sub(previous.cpu_stats.cpu_usage.total_usage)?;
        let system = self
            .cpu_stats
            .system_cpu_usage?
            .checked_sub(previous.cpu_stats.system_cpu_usage?)?;
        let cores = self.cpu_stats.online_cpus.unwrap_or(
            self.cpu_stats
                .cpu_usage
                .percpu_usage
                .as_ref()
                .map_or(0, |v| v.len() as u64),
        );
        (system > 0 && cores > 0).then(|| cpu as f64 / system as f64 * cores as f64 * 100.)
    }
    pub fn memory_usage(&self) -> Option<u64> {
        let used = self.memory_stats.usage?;
        let cache = self
            .memory_stats
            .stats
            .get("total_inactive_file")
            .or_else(|| self.memory_stats.stats.get("inactive_file"))
            .copied()
            .unwrap_or(0);
        Some(if cache < used { used - cache } else { used })
    }
    pub fn network_bytes(&self) -> (u64, u64) {
        self.networks.values().fold((0u64, 0u64), |(rx, tx), n| {
            (rx.saturating_add(n.rx_bytes), tx.saturating_add(n.tx_bytes))
        })
    }
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct CpuStats {
    #[serde(default)]
    pub cpu_usage: CpuUsage,
    pub system_cpu_usage: Option<u64>,
    pub online_cpus: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct CpuUsage {
    #[serde(default)]
    pub total_usage: u64,
    pub percpu_usage: Option<Vec<u64>>,
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct MemoryStats {
    pub usage: Option<u64>,
    pub limit: Option<u64>,
    #[serde(default, deserialize_with = "null_default")]
    pub stats: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct NetworkStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}
#[derive(Clone, Debug, Deserialize, Default)]
pub struct PidStats {
    pub current: Option<u64>,
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}
