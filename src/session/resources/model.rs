use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ResourceSnapshot {
    pub sampled_at: Instant,
    pub cpu_percent: Option<f64>,
    pub cpu_count: u32,
    pub load_average: [f64; 3],
    pub uptime: Duration,
    pub memory: MemoryUsage,
    pub networks: Vec<NetworkUsage>,
    pub disks: Vec<DiskUsage>,
    pub disks_sampled_at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct MemoryUsage {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_available_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct NetworkUsage {
    pub name: String,
    pub rx_bytes_per_sec: Option<f64>,
    pub tx_bytes_per_sec: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct DiskUsage {
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    /// Like df's Use%: reserved blocks are excluded from the usable denominator.
    pub used_percent: f64,
}
