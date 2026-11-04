use crate::i18n::messages as i18n_key;

use super::model::{DiskUsage, MemoryUsage, NetworkUsage, ResourceSnapshot};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_NETWORKS: usize = 256;
const MAX_DISKS: usize = 32;
const MAX_RATE_GAP: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub(super) enum ParseError {
    UnsupportedPlatform,
    Invalid(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str(crate::i18n::tr(
                i18n_key::RESOURCES_RESOURCE_MONITORING_REQUIRES_LINUX_PROCFS,
            )),
            Self::Invalid(message) => f.write_str(crate::i18n::tr(message)),
        }
    }
}

impl std::error::Error for ParseError {}

struct RawSample {
    boot_id: String,
    cpu: [u64; 8],
    cpu_count: u32,
    uptime: Duration,
    load_average: [f64; 3],
    memory: MemoryUsage,
    networks: HashMap<String, [u64; 2]>,
    disks: Option<Vec<DiskUsage>>,
}

struct Previous {
    sampled_at: Instant,
    uptime: Duration,
    cpu: [u64; 8],
    cpu_count: u32,
    networks: HashMap<String, [u64; 2]>,
}

#[derive(Default)]
pub(super) struct Sampler {
    boot_id: Option<String>,
    previous: Option<Previous>,
    disks: Vec<DiskUsage>,
    disks_sampled_at: Option<Instant>,
}

impl Sampler {
    pub(super) fn reset_rates(&mut self) {
        self.previous = None;
    }

    pub(super) fn sample(
        &mut self,
        output: &[u8],
        sampled_at: Instant,
    ) -> Result<ResourceSnapshot, ParseError> {
        let raw = match parse(output) {
            Ok(raw) => raw,
            Err(error) => {
                self.reset_rates();
                return Err(error);
            }
        };
        let same_boot = self.boot_id.as_ref() == Some(&raw.boot_id);
        if !same_boot {
            self.disks.clear();
            self.disks_sampled_at = None;
        }
        let previous = self.previous.as_ref().filter(|previous| {
            same_boot
                && raw.uptime > previous.uptime
                && sampled_at
                    .checked_duration_since(previous.sampled_at)
                    .is_some_and(|gap| !gap.is_zero() && gap <= MAX_RATE_GAP)
        });
        let cpu_percent = previous
            .filter(|previous| previous.cpu_count == raw.cpu_count)
            .and_then(|previous| cpu_percent(&previous.cpu, &raw.cpu));
        let mut networks: Vec<_> = raw
            .networks
            .iter()
            .map(|(name, counters)| {
                let rates = previous.and_then(|previous| {
                    let before = previous.networks.get(name)?;
                    let rx = counters[0].checked_sub(before[0])?;
                    let tx = counters[1].checked_sub(before[1])?;
                    let seconds = (raw.uptime - previous.uptime).as_secs_f64();
                    Some((rx as f64 / seconds, tx as f64 / seconds))
                });
                NetworkUsage {
                    name: name.clone(),
                    rx_bytes_per_sec: rates.map(|rates| rates.0),
                    tx_bytes_per_sec: rates.map(|rates| rates.1),
                }
            })
            .collect();
        networks.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        if let Some(disks) = raw.disks {
            self.disks = disks;
            self.disks_sampled_at = Some(sampled_at);
        }
        self.previous = Some(Previous {
            sampled_at,
            uptime: raw.uptime,
            cpu: raw.cpu,
            cpu_count: raw.cpu_count,
            networks: raw.networks,
        });
        self.boot_id = Some(raw.boot_id);
        Ok(ResourceSnapshot {
            sampled_at,
            cpu_percent,
            cpu_count: raw.cpu_count,
            load_average: raw.load_average,
            uptime: raw.uptime,
            memory: raw.memory,
            networks,
            disks: self.disks.clone(),
            disks_sampled_at: self.disks_sampled_at,
        })
    }
}

fn cpu_percent(before: &[u64; 8], after: &[u64; 8]) -> Option<f64> {
    let mut deltas = [0; 8];
    for (index, delta) in deltas.iter_mut().enumerate() {
        *delta = after[index].checked_sub(before[index])?;
    }
    // user/nice already include guest time. Only the first eight counters count.
    let total = deltas
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))?;
    let idle = deltas[3].checked_add(deltas[4])?;
    (total > 0).then(|| (total - idle) as f64 * 100.0 / total as f64)
}

fn number(value: &str) -> Result<u64, ParseError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_INVALID_RESOURCE_COUNTER,
        ));
    }
    value
        .parse()
        .map_err(|_| ParseError::Invalid(i18n_key::RESOURCES_RESOURCE_COUNTER_EXCEEDS_ITS_LIMIT))
}

fn decimal(value: &str) -> Result<f64, ParseError> {
    let number: f64 = value
        .parse()
        .map_err(|_| ParseError::Invalid(i18n_key::RESOURCES_INVALID_RESOURCE_MEASUREMENT))?;
    if !number.is_finite() || number < 0.0 {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_INVALID_RESOURCE_MEASUREMENT,
        ));
    }
    Ok(number)
}

fn unique<T>(slot: &mut Option<T>, value: T) -> Result<(), ParseError> {
    if slot.replace(value).is_some() {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_DUPLICATE_RESOURCE_MEASUREMENT,
        ));
    }
    Ok(())
}

fn parse(output: &[u8]) -> Result<RawSample, ParseError> {
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_RESOURCE_OUTPUT_EXCEEDS_ITS_LIMIT,
        ));
    }
    if output == b"RTRES_UNSUPPORTED\n" {
        return Err(ParseError::UnsupportedPlatform);
    }
    let text = String::from_utf8_lossy(output);
    let mut lines = text.lines();
    if lines.next() != Some("RTRES\t1") {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_INVALID_RESOURCE_OUTPUT_HEADER,
        ));
    }
    let (mut boot_id, mut cpu, mut memory, mut uptime, mut load_average) =
        (None, None, None, None, None);
    let mut networks = HashMap::new();
    let mut disks = None;
    let mut mounts = HashSet::new();
    let mut complete = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if complete || line.len() > 16 * 1024 + 128 {
            return Err(ParseError::Invalid(
                i18n_key::RESOURCES_INVALID_RESOURCE_OUTPUT_FRAMING,
            ));
        }
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["BOOT", value] => {
                let id = uuid::Uuid::parse_str(value).map_err(|_| {
                    ParseError::Invalid(i18n_key::RESOURCES_INVALID_BOOT_IDENTIFIER)
                })?;
                unique(&mut boot_id, id.to_string())?;
            }
            ["UPTIME", value] => unique(
                &mut uptime,
                Duration::try_from_secs_f64(decimal(value)?)
                    .map_err(|_| ParseError::Invalid(i18n_key::RESOURCES_INVALID_UPTIME))?,
            )?,
            ["LOAD", first, second, third] => unique(
                &mut load_average,
                [decimal(first)?, decimal(second)?, decimal(third)?],
            )?,
            [
                "CPU",
                count,
                user,
                nice,
                system,
                idle,
                iowait,
                irq,
                softirq,
                steal,
            ] => {
                let count = u32::try_from(number(count)?)
                    .map_err(|_| ParseError::Invalid(i18n_key::RESOURCES_INVALID_CPU_COUNT))?;
                if count == 0 {
                    return Err(ParseError::Invalid(i18n_key::RESOURCES_INVALID_CPU_COUNT));
                }
                unique(
                    &mut cpu,
                    (
                        count,
                        [
                            number(user)?,
                            number(nice)?,
                            number(system)?,
                            number(idle)?,
                            number(iowait)?,
                            number(irq)?,
                            number(softirq)?,
                            number(steal)?,
                        ],
                    ),
                )?;
            }
            ["MEM", total, available, swap_total, swap_available] => {
                let kib = |value| {
                    number(value)?.checked_mul(1024).ok_or(ParseError::Invalid(
                        i18n_key::RESOURCES_MEMORY_SIZE_EXCEEDS_ITS_LIMIT,
                    ))
                };
                let (total, available, swap_total, swap_available) = (
                    kib(total)?,
                    kib(available)?,
                    kib(swap_total)?,
                    kib(swap_available)?,
                );
                if total == 0 || available > total || swap_available > swap_total {
                    return Err(ParseError::Invalid(
                        i18n_key::RESOURCES_INCONSISTENT_MEMORY_COUNTERS,
                    ));
                }
                unique(
                    &mut memory,
                    MemoryUsage {
                        total_bytes: total,
                        available_bytes: available,
                        used_bytes: total - available,
                        swap_total_bytes: swap_total,
                        swap_available_bytes: swap_available,
                        swap_used_bytes: swap_total - swap_available,
                    },
                )?;
            }
            ["NET", name, rx, tx] => {
                if name.is_empty()
                    || name.len() > 64
                    || name.chars().any(|ch| ch.is_control() || ch.is_whitespace())
                    || networks.len() >= MAX_NETWORKS
                    || networks
                        .insert((*name).into(), [number(rx)?, number(tx)?])
                        .is_some()
                {
                    return Err(ParseError::Invalid(
                        i18n_key::RESOURCES_INVALID_NETWORK_INTERFACE_COUNTERS,
                    ));
                }
            }
            ["DISKS"] => unique(&mut disks, Vec::new())?,
            ["DISK", path, block_size, blocks, free, available] => {
                let disks = disks.as_mut().ok_or(ParseError::Invalid(
                    i18n_key::RESOURCES_MISSING_FILESYSTEM_SECTION,
                ))?;
                let mount_point = mount_point(path)?;
                if disks.len() >= MAX_DISKS || !mounts.insert(mount_point.clone()) {
                    return Err(ParseError::Invalid(
                        i18n_key::RESOURCES_INVALID_FILESYSTEM_LIST,
                    ));
                }
                let size = number(block_size)?;
                let total = number(blocks)?;
                let free = number(free)?;
                let available = number(available)?;
                if size == 0 || total == 0 || free > total || available > free {
                    return Err(ParseError::Invalid(
                        i18n_key::RESOURCES_INCONSISTENT_FILESYSTEM_COUNTERS,
                    ));
                }
                let bytes = |blocks: u64| {
                    blocks.checked_mul(size).ok_or(ParseError::Invalid(
                        i18n_key::RESOURCES_FILESYSTEM_SIZE_EXCEEDS_ITS_LIMIT,
                    ))
                };
                let used = total - free;
                let usable = used as f64 + available as f64;
                disks.push(DiskUsage {
                    mount_point,
                    total_bytes: bytes(total)?,
                    available_bytes: bytes(available)?,
                    used_bytes: bytes(used)?,
                    used_percent: if usable > 0.0 {
                        used as f64 * 100.0 / usable
                    } else {
                        0.0
                    },
                });
            }
            ["END"] => complete = true,
            _ => {
                return Err(ParseError::Invalid(
                    i18n_key::RESOURCES_UNKNOWN_RESOURCE_OUTPUT_RECORD,
                ));
            }
        }
    }
    if !complete {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_INCOMPLETE_RESOURCE_OUTPUT,
        ));
    }
    let missing = || ParseError::Invalid(i18n_key::RESOURCES_MISSING_RESOURCE_MEASUREMENT);
    let (cpu_count, cpu) = cpu.ok_or_else(missing)?;
    Ok(RawSample {
        boot_id: boot_id.ok_or_else(missing)?,
        cpu,
        cpu_count,
        memory: memory.ok_or_else(missing)?,
        uptime: uptime.ok_or_else(missing)?,
        load_average: load_average.ok_or_else(missing)?,
        networks,
        disks,
    })
}

fn mount_point(encoded: &str) -> Result<String, ParseError> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut input = encoded.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = [input.next(), input.next(), input.next()];
        bytes.push(match escaped {
            [Some(b'0'), Some(b'4'), Some(b'0')] => b' ',
            [Some(b'0'), Some(b'1'), Some(b'1')] => b'\t',
            [Some(b'0'), Some(b'1'), Some(b'2')] => b'\n',
            [Some(b'1'), Some(b'3'), Some(b'4')] => b'\\',
            _ => {
                return Err(ParseError::Invalid(
                    i18n_key::RESOURCES_INVALID_FILESYSTEM_MOUNT_POINT,
                ));
            }
        });
    }
    if bytes.first() != Some(&b'/') || bytes.contains(&0) || bytes.len() > 4096 {
        return Err(ParseError::Invalid(
            i18n_key::RESOURCES_INVALID_FILESYSTEM_MOUNT_POINT,
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "89f3f826-8105-46e4-8e03-f70f1cec80bd";

    fn sample(uptime: u64, user: u64, idle: u64, rx: u64, tx: u64, disks: &str) -> Vec<u8> {
        format!("RTRES\t1\nBOOT\t{BOOT}\nUPTIME\t{uptime}.00\nLOAD\t0.25\t1.5\t2.0\nCPU\t2\t{user}\t0\t0\t{idle}\t0\t0\t0\t0\nMEM\t1000\t400\t100\t25\nNET\teth0\t{rx}\t{tx}\n{disks}END\n").into_bytes()
    }

    #[test]
    fn samples_compute_counter_deltas_and_retain_disk_collection_time() {
        let now = Instant::now();
        let mut sampler = Sampler::default();
        let first = sampler
            .sample(
                &sample(
                    100,
                    10,
                    90,
                    1000,
                    2000,
                    "DISKS\nDISK\t/data\\040space\\134name\\012\t4096\t100\t25\t20\n",
                ),
                now,
            )
            .unwrap();
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.networks[0].rx_bytes_per_sec, None);
        assert_eq!(first.memory.used_bytes, 600 * 1024);
        assert_eq!(first.memory.swap_used_bytes, 75 * 1024);
        assert_eq!(first.disks[0].mount_point, "/data space\\name\n");
        assert_eq!(first.disks[0].used_bytes, 75 * 4096);
        assert!((first.disks[0].used_percent - 75.0 / 95.0 * 100.0).abs() < 0.0001);
        let next = now + Duration::from_secs(3);
        let second = sampler
            .sample(&sample(103, 30, 170, 1300, 2600, ""), next)
            .unwrap();
        assert_eq!(second.cpu_percent, Some(20.0));
        assert_eq!(second.networks[0].rx_bytes_per_sec, Some(100.0));
        assert_eq!(second.networks[0].tx_bytes_per_sec, Some(200.0));
        assert_eq!(second.disks_sampled_at, Some(now));
        assert_eq!(second.disks.len(), 1);
        sampler.reset_rates();
        let resumed = sampler
            .sample(
                &sample(106, 50, 250, 1600, 3200, "DISKS\n"),
                next + Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(resumed.cpu_percent, None);
        assert_eq!(resumed.networks[0].rx_bytes_per_sec, None);
        assert!(resumed.disks.is_empty());
        assert_eq!(
            resumed.disks_sampled_at,
            Some(next + Duration::from_secs(3))
        );
    }

    #[test]
    fn counter_resets_reboots_and_sampling_gaps_do_not_fabricate_zero_rates() {
        let now = Instant::now();
        let mut sampler = Sampler::default();
        sampler
            .sample(
                &sample(
                    100,
                    10,
                    90,
                    1000,
                    2000,
                    "DISKS\nDISK\t/\t4096\t100\t25\t20\n",
                ),
                now,
            )
            .unwrap();
        let reset = sampler
            .sample(
                &sample(103, 1, 100, 10, 2500, ""),
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(reset.cpu_percent, None);
        assert_eq!(reset.networks[0].rx_bytes_per_sec, None);
        assert_eq!(reset.networks[0].tx_bytes_per_sec, None);
        let reboot = String::from_utf8(sample(106, 100, 200, 2000, 3000, ""))
            .unwrap()
            .replace(BOOT, "da5960bc-e7f8-40f0-8415-28c464dd9e43");
        let restarted = sampler
            .sample(reboot.as_bytes(), now + Duration::from_secs(6))
            .unwrap();
        assert_eq!(restarted.cpu_percent, None);
        assert_eq!(restarted.networks[0].rx_bytes_per_sec, None);
        assert!(restarted.disks.is_empty());
        assert_eq!(restarted.disks_sampled_at, None);
        let later = reboot
            .replace("106.00", "166.00")
            .replace("\t100\t0", "\t110\t0");
        let resumed = sampler
            .sample(later.as_bytes(), now + Duration::from_secs(66))
            .unwrap();
        assert_eq!(resumed.cpu_percent, None);
        assert_eq!(resumed.networks[0].rx_bytes_per_sec, None);
    }

    #[test]
    fn malformed_or_partial_samples_reset_only_the_rate_baseline() {
        let valid = String::from_utf8(sample(100, 10, 90, 1000, 2000, "")).unwrap();
        for invalid in [
            valid.replace("END\n", ""),
            valid.replace("0.25", "NaN"),
            valid.replace("MEM\t1000", "MEM\t18446744073709551615"),
            valid.replace("CPU\t2", "CPU\t0"),
            valid.replace("END\n", "NET\teth0\t1\t2\nEND\n"),
            valid.replace(
                "END\n",
                "DISKS\nDISK\t/invalid\\000\t4096\t100\t25\t20\nEND\n",
            ),
        ] {
            assert!(parse(invalid.as_bytes()).is_err());
        }
        assert!(matches!(
            parse(b"RTRES_UNSUPPORTED\n"),
            Err(ParseError::UnsupportedPlatform)
        ));
        assert!(parse(&vec![b'x'; MAX_OUTPUT_BYTES + 1]).is_err());
        let now = Instant::now();
        let mut sampler = Sampler::default();
        sampler.sample(valid.as_bytes(), now).unwrap();
        assert!(
            sampler
                .sample(b"partial", now + Duration::from_secs(1))
                .is_err()
        );
        let recovered = sampler
            .sample(
                &sample(103, 30, 170, 1300, 2600, ""),
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert_eq!(recovered.cpu_percent, None);
        assert_eq!(recovered.networks[0].rx_bytes_per_sec, None);
    }
}
