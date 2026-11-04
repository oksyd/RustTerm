//! Runtime-independent RDP UDP transports (MS-RDPEUDP and MS-RDPEUDP2).
//!
//! Supply UDP packets and a monotonic millisecond clock, then drain outgoing
//! datagrams and events. Reliable payloads form a TLS byte stream; lossy
//! payloads preserve DTLS datagram boundaries. This layer supplies no encryption.

mod fec;
mod wire;

use crate::{Error, Result};
use std::collections::{BTreeMap, VecDeque};
use wire::{MTU, WINDOW};

/// Delivery semantics requested by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Ordered delivery, retransmission and congestion control.
    Reliable,
    /// Ordered datagrams with forward error correction, without retransmission.
    Lossy,
}

/// Negotiated UDP wire protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// MS-RDPEUDP with a 500 ms minimum retransmission timeout.
    V1,
    /// MS-RDPEUDP with a 300 ms minimum retransmission timeout.
    V2,
    /// MS-RDPEUDP2, negotiated as version 0x0101.
    V3,
}

/// Parameters supplied by the authenticated TCP connection.
#[derive(Clone)]
pub struct Config {
    /// Cryptographically random initial sequence number.
    pub initial_sequence: u32,
    /// Delivery mode in the server's multitransport request.
    pub mode: Mode,
    /// SHA-256 of the security cookie; enables version 3 in reliable mode.
    pub cookie_hash: Option<[u8; 32]>,
}

/// A transport event for the TLS/DTLS adapter.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The three-way UDP handshake completed.
    Connected,
    /// Ordered stream bytes, or one lossy datagram depending on the mode.
    Data(Vec<u8>),
    /// The handshake, retransmission or idle deadline expired.
    Closed,
}

struct Sent {
    bytes: Vec<u8>,
    coded: Option<u64>,
    first_sent: u64,
    last_sent: u64,
    retries: u8,
    later_acked: usize,
}

/// A bounded client-side UDP connection, independent of sockets and runtimes.
pub struct Connection {
    config: Config,
    version: Option<Version>,
    peer_initial: Option<u32>,
    peer_window: usize,
    mtu: usize,
    next_source: u64,
    next_coded: u64,
    receive_next: u64,
    receive_highest: u64,
    code_floor: u64,
    code_highest: u64,
    code_received: BTreeMap<u64, u64>,
    code_base_known: bool,
    code_cumulative_safe: bool,
    code_ack_override: Option<u64>,
    received: BTreeMap<u64, Vec<u8>>,
    reorder_deadline: Option<u64>,
    recovery: fec::Recovery,
    ack_floor: u64,
    pending: VecDeque<Vec<u8>>,
    inflight: BTreeMap<u64, Sent>,
    controls: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
    queued_data: usize,
    congestion_window: usize,
    slow_start_threshold: usize,
    congestion_credit: usize,
    srtt: Option<u64>,
    variation: u64,
    first_syn: u64,
    last_syn: u64,
    syn_attempts: u8,
    last_receive: u64,
    last_send: u64,
    ack_pending: bool,
    congestion_pending: bool,
    closed: bool,
}

impl Connection {
    /// Begins a handshake. Drain [`Self::poll_transmit`] to send the initial SYN.
    pub fn new(config: Config, now_ms: u64) -> Self {
        let initial = u64::from(config.initial_sequence);
        let syn = wire::syn(config.initial_sequence, config.mode, config.cookie_hash);
        Self {
            config,
            version: None,
            peer_initial: None,
            peer_window: WINDOW,
            mtu: MTU,
            next_source: initial + 1,
            next_coded: initial + 1,
            receive_next: 0,
            receive_highest: 0,
            code_floor: 0,
            code_highest: 0,
            code_received: BTreeMap::new(),
            code_base_known: false,
            code_cumulative_safe: false,
            code_ack_override: None,
            received: BTreeMap::new(),
            reorder_deadline: None,
            recovery: fec::Recovery::new(),
            ack_floor: 0,
            pending: VecDeque::new(),
            inflight: BTreeMap::new(),
            controls: VecDeque::from([syn]),
            events: VecDeque::new(),
            queued_data: 0,
            congestion_window: 4,
            slow_start_threshold: 64,
            congestion_credit: 0,
            srtt: None,
            variation: 0,
            first_syn: now_ms,
            last_syn: now_ms,
            syn_attempts: 1,
            last_receive: now_ms,
            last_send: now_ms,
            ack_pending: false,
            congestion_pending: false,
            closed: false,
        }
    }

    /// The selected wire version, once the UDP handshake completes.
    pub fn version(&self) -> Option<Version> {
        self.version
    }

    /// Whether payloads may be queued for transport.
    pub fn is_established(&self) -> bool {
        self.version.is_some() && !self.closed
    }

    /// Whether the connection has terminated.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Maximum payload bytes in one packet, with space for feedback and FEC.
    pub fn max_payload(&self) -> usize {
        self.mtu
            - if self.version == Some(Version::V3) {
                160
            } else {
                288
            }
    }

    /// Queues bytes atomically; reliable streams are split into bounded packets.
    ///
    /// # Errors
    /// Rejects an unestablished connection, oversized lossy datagram or full
    /// send queue. No bytes are retained when an error is returned.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.is_established() {
            return Err(Error::Invalid("UDP connection state"));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let payload = self.max_payload();
        let chunks = bytes.len().div_ceil(payload);
        if (self.config.mode == Mode::Lossy && chunks > 1)
            || chunks > WINDOW.saturating_sub(self.pending.len())
        {
            return Err(Error::Limit("UDP send queue"));
        }
        self.pending
            .extend(bytes.chunks(payload).map(<[u8]>::to_vec));
        Ok(())
    }

    /// Processes one network datagram.
    ///
    /// # Errors
    /// Returns an error for invalid packet encoding, negotiation or FEC recovery.
    /// Invalid wire headers do not reset the idle timer. FEC recovery errors can
    /// occur after valid headers have already updated the connection state.
    pub fn receive(&mut self, bytes: &[u8], now_ms: u64) -> Result<()> {
        if self.closed {
            return Err(Error::Invalid("closed UDP connection"));
        }
        let looks_like_syn = bytes.len() >= 16
            && u16::from_be_bytes([bytes[6], bytes[7]]) & (wire::SYN | wire::ACK)
                == wire::SYN | wire::ACK;
        if self.version.is_none() || looks_like_syn {
            let syn = wire::syn_ack(
                bytes,
                self.config.initial_sequence,
                self.config.mode,
                self.config.cookie_hash.is_some(),
            )?;
            if let Some(initial) = self.peer_initial {
                if initial != syn.initial || self.version != Some(syn.version) {
                    return Err(Error::Invalid("changed UDP handshake"));
                }
            } else {
                self.peer_initial = Some(syn.initial);
                self.version = Some(syn.version);
                self.peer_window = syn.window;
                self.mtu = syn.mtu;
                self.code_floor = u64::from(syn.initial) + 1;
                self.code_highest = u64::from(syn.initial);
                if syn.version == Version::V3 {
                    // UDP2 starts a separate sequence space. Its first DATA/AOA,
                    // not the legacy SYN ISN, establishes the peer's coded base.
                    self.code_floor = 1 << 16;
                    self.code_highest = 1 << 16;
                }
                self.receive_next = if syn.version == Version::V3 {
                    1
                } else {
                    self.code_floor
                };
                self.next_source = if syn.version == Version::V3 {
                    1
                } else {
                    self.next_source
                };
                self.receive_highest = self.receive_next - 1;
                self.ack_floor = self.receive_highest;
                self.recovery.insert(self.receive_highest, Vec::new());
                self.sample_rtt(now_ms.saturating_sub(self.first_syn).max(1));
                self.events.push_back(Event::Connected);
            }
            self.last_receive = now_ms;
            if syn.version != Version::V3 && self.controls.len() < 4 {
                self.controls.push_back(wire::final_ack(syn.initial));
            }
            return Ok(());
        }
        let modern = self.version == Some(Version::V3);
        let packet = if modern {
            wire::modern(
                bytes,
                self.next_coded - 1,
                self.receive_next,
                self.code_highest,
            )?
        } else {
            wire::legacy(
                bytes,
                self.next_source - 1,
                self.receive_next,
                self.code_highest,
            )?
        };
        if modern
            && !packet.dummy
            && packet
                .source
                .as_ref()
                .is_some_and(|source| source.source as u16 == 0)
        {
            return Err(Error::Invalid("zero UDP2 channel sequence"));
        }
        self.peer_window = packet.window;
        self.last_receive = now_ms;
        self.acknowledge(&packet, now_ms);
        if packet.congestion {
            self.reduce_congestion();
        }
        if let Some(reset) = packet.reset {
            if modern
                && (!self.code_base_known
                    || (reset > self.code_floor
                        && reset <= self.code_highest.saturating_add(32767)))
            {
                self.code_floor = reset;
                self.code_highest = if self.code_base_known {
                    self.code_highest.max(reset.saturating_sub(1))
                } else {
                    reset.saturating_sub(1)
                };
                self.code_base_known = true;
                self.code_cumulative_safe = true;
                while self.code_received.contains_key(&self.code_floor) {
                    self.code_floor += 1;
                }
            } else if !modern && reset <= self.receive_highest {
                self.ack_floor = self.ack_floor.max(reset);
            }
        }
        if let Some(source) = packet.source {
            if !packet.dummy
                && source.source >= self.receive_next
                && !self.received.contains_key(&source.source)
                && (source.source >= self.receive_next.saturating_add(WINDOW as u64)
                    || self.received.len() + self.queued_data >= WINDOW)
            {
                // Never acknowledge a coded packet whose channel payload could
                // not be retained; doing so would defeat reliable retransmission.
                return Ok(());
            }
            if modern {
                if !self.code_base_known {
                    self.code_floor = source.coded;
                    self.code_highest = source.coded;
                    self.code_base_known = true;
                    self.code_cumulative_safe = source.source == 1;
                } else if !self.code_cumulative_safe && source.source == 1 {
                    self.code_floor = self.code_floor.min(source.coded);
                    self.code_cumulative_safe = true;
                }
                if source.coded < self.code_floor {
                    self.code_ack_override = Some(source.coded);
                }
                self.code_received.insert(source.coded, now_ms);
                self.code_highest = self.code_highest.max(source.coded);
                while self.code_received.contains_key(&self.code_floor) {
                    self.code_floor += 1;
                }
                while self.code_received.len() > WINDOW {
                    self.code_received.pop_first();
                }
            }
            if !packet.dummy {
                self.accept(source.source, source.bytes, now_ms);
            }
            self.ack_pending = true;
        }
        if let Some(parity) = packet.parity {
            for (sequence, bytes) in self.recovery.parity(parity, self.receive_next)? {
                self.accept(sequence, bytes, now_ms);
            }
            self.ack_pending = true;
        } else if !modern {
            for (sequence, bytes) in self.recovery.recover(self.receive_next)? {
                self.accept(sequence, bytes, now_ms);
            }
        }
        Ok(())
    }

    /// Drains one event. Receiving data releases its advertised receive capacity.
    pub fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if matches!(event, Event::Data(_)) {
            self.queued_data -= 1;
            self.ack_pending = true;
        }
        Some(event)
    }

    /// Produces the next network datagram while advancing protocol timers.
    ///
    /// Call repeatedly until `None`, then wait for input or [`Self::next_deadline`].
    pub fn poll_transmit(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        if self.closed {
            return None;
        }
        if now_ms.saturating_sub(self.last_receive) >= self.idle_timeout() {
            self.close();
            return None;
        }
        if self
            .reorder_deadline
            .is_some_and(|deadline| now_ms >= deadline)
        {
            self.release_received(true);
        }
        if self.version.is_none() && now_ms.saturating_sub(self.last_syn) >= 500 {
            if self.syn_attempts >= 6 {
                self.close();
                return None;
            }
            self.syn_attempts += 1;
            self.last_syn = now_ms;
            self.controls.push_back(wire::syn(
                self.config.initial_sequence,
                self.config.mode,
                self.config.cookie_hash,
            ));
        }
        if let Some(packet) = self.controls.pop_front() {
            self.last_send = now_ms;
            return Some(packet);
        }
        self.version?;
        let retransmit = self.inflight.iter().find_map(|(source, packet)| {
            (packet.later_acked >= 3
                || now_ms.saturating_sub(packet.last_sent) >= self.rto(packet.retries))
            .then_some(*source)
        });
        if let Some(source) = retransmit {
            if self.config.mode == Mode::Lossy {
                self.inflight.remove(&source);
                self.reduce_congestion();
            } else {
                let packet = self.inflight.get_mut(&source)?;
                if packet.retries >= 5 {
                    self.close();
                    return None;
                }
                packet.retries += 1;
                packet.later_acked = 0;
                packet.last_sent = now_ms;
                let bytes = packet.bytes.clone();
                self.reduce_congestion();
                return Some(self.source_packet(source, &bytes, now_ms));
            }
        }
        if self.inflight.len() < self.congestion_window.min(self.peer_window)
            && let Some(bytes) = self.pending.pop_front()
        {
            let source = self.next_source;
            self.next_source += 1;
            if self.version == Some(Version::V3) && self.next_source as u16 == 0 {
                self.next_source += 1;
            }
            self.inflight.insert(
                source,
                Sent {
                    bytes: bytes.clone(),
                    coded: None,
                    first_sent: now_ms,
                    last_sent: now_ms,
                    retries: 0,
                    later_acked: 0,
                },
            );
            let packet = self.source_packet(source, &bytes, now_ms);
            if self.version != Some(Version::V3) && self.config.mode == Mode::Lossy {
                let mut parity = self.legacy_header(wire::DATA | wire::FEC | wire::AOA);
                self.append_aoa(&mut parity);
                self.next_coded += 1;
                parity.extend_from_slice(&((self.next_coded - 1) as u32).to_be_bytes());
                parity.extend_from_slice(&(source as u32).to_be_bytes());
                let index = source.wrapping_add(1) as u8;
                parity.extend_from_slice(&[0, index, 0, 0]);
                parity.extend_from_slice(&fec::encode(&[(source, bytes.as_slice())], index));
                self.controls.push_back(parity);
            }
            return Some(packet);
        }
        if self.ack_pending || now_ms.saturating_sub(self.last_send) >= 8000 {
            self.ack_pending = false;
            self.last_send = now_ms;
            let packet = if self.version == Some(Version::V3) {
                wire::wrap_modern(&self.modern_header(None, now_ms))
            } else {
                let mut packet = self.legacy_header(wire::AOA);
                self.append_aoa(&mut packet);
                packet
            };
            self.code_ack_override = None;
            return Some(packet);
        }
        None
    }

    /// Earliest millisecond deadline requiring another poll, on the caller's clock.
    pub fn next_deadline(&self) -> u64 {
        let mut deadline = self.last_receive.saturating_add(self.idle_timeout());
        if let Some(reorder) = self.reorder_deadline {
            deadline = deadline.min(reorder);
        }
        if self.version.is_none() {
            deadline = deadline.min(self.last_syn.saturating_add(500));
        } else {
            deadline = deadline.min(self.last_send.saturating_add(8000));
            for packet in self.inflight.values() {
                deadline = deadline.min(packet.last_sent.saturating_add(self.rto(packet.retries)));
            }
        }
        deadline
    }

    /// Ends the local transport and releases queued payloads.
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.pending.clear();
            self.inflight.clear();
            self.controls.clear();
            self.received.clear();
            self.reorder_deadline = None;
            self.recovery = fec::Recovery::new();
            self.events.push_back(Event::Closed);
        }
    }

    fn idle_timeout(&self) -> u64 {
        if self.version == Some(Version::V3) {
            16_000
        } else {
            65_000
        }
    }

    fn sample_rtt(&mut self, sample: u64) {
        if let Some(srtt) = self.srtt {
            self.variation = (self.variation.saturating_mul(3) + srtt.abs_diff(sample)) / 4;
            self.srtt = Some((srtt.saturating_mul(7) + sample) / 8);
        } else {
            self.srtt = Some(sample);
            self.variation = sample / 2;
        }
    }

    fn rto(&self, retries: u8) -> u64 {
        let minimum = match self.version {
            Some(Version::V1) => 500,
            Some(Version::V2) => 300,
            _ => 100,
        };
        let base = self
            .srtt
            .unwrap_or(500)
            .saturating_add(self.variation.saturating_mul(4))
            .max(minimum);
        let base = if self.version == Some(Version::V3) {
            base
        } else {
            base.max(self.srtt.unwrap_or(500).saturating_mul(2))
        };
        base.saturating_mul(1u64 << retries.min(4)).min(8000)
    }

    fn reduce_congestion(&mut self) {
        self.slow_start_threshold = (self.congestion_window / 2).max(2);
        self.congestion_window = self.slow_start_threshold;
        self.congestion_credit = 0;
        self.congestion_pending = true;
    }

    fn acknowledge(&mut self, packet: &wire::Packet, now_ms: u64) {
        let modern = self.version == Some(Version::V3);
        let maximum = if modern {
            self.next_coded - 1
        } else {
            self.next_source - 1
        };
        let acknowledged = |sequence: u64| {
            packet
                .cumulative
                .is_some_and(|last| last <= maximum && sequence <= last)
                || packet
                    .acknowledgements
                    .iter()
                    .any(|(first, last)| *last <= maximum && (*first..=*last).contains(&sequence))
        };
        let completed: Vec<_> = self
            .inflight
            .iter()
            .filter_map(|(source, sent)| {
                let done = if modern {
                    sent.coded.is_some_and(acknowledged)
                } else {
                    acknowledged(*source)
                };
                done.then_some(*source)
            })
            .collect();
        let newest = completed.iter().copied().max();
        for source in completed {
            if let Some(sent) = self.inflight.remove(&source) {
                if sent.retries == 0 {
                    self.sample_rtt(now_ms.saturating_sub(sent.first_sent).max(1));
                }
                if self.congestion_window < self.slow_start_threshold {
                    self.congestion_window += 1;
                } else {
                    self.congestion_credit += 1;
                    if self.congestion_credit >= self.congestion_window {
                        self.congestion_window += 1;
                        self.congestion_credit = 0;
                    }
                }
                self.congestion_window = self.congestion_window.min(WINDOW);
                for (older, pending) in &mut self.inflight {
                    if *older < source {
                        pending.later_acked += 1;
                    }
                }
            }
        }
        if newest.is_some() {
            self.ack_pending |= self.peer_window == 0;
        }
    }

    fn accept(&mut self, source: u64, bytes: Vec<u8>, now_ms: u64) {
        if source < self.receive_next
            || source >= self.receive_next.saturating_add(WINDOW as u64)
            || self.received.contains_key(&source)
            || self.received.len() + self.queued_data >= WINDOW
        {
            return;
        }
        self.receive_highest = self.receive_highest.max(source);
        self.recovery.insert(source, bytes.clone());
        self.received.insert(source, bytes);
        let lossy = self.config.mode == Mode::Lossy;
        let at_right_edge = source == self.receive_next.saturating_add(WINDOW as u64 - 1);
        self.release_received(lossy && at_right_edge);
        if lossy && !self.received.is_empty() {
            // MS-RDPEUDP 3.1.1.1 requires ordered delivery even without ARQ.
            // Give reordered packets/FEC a bounded opportunity to fill the gap.
            self.reorder_deadline
                .get_or_insert(now_ms.saturating_add(50));
        }
        self.ack_pending = true;
    }

    fn release_received(&mut self, skip_missing: bool) {
        while let Some((&source, _)) = self.received.first_key_value() {
            if source != self.receive_next && !skip_missing {
                break;
            }
            let (_, bytes) = self.received.pop_first().expect("received first key");
            self.receive_next = source + 1;
            if self.version == Some(Version::V3) && self.receive_next as u16 == 0 {
                self.receive_next += 1;
            }
            self.events.push_back(Event::Data(bytes));
            self.queued_data += 1;
        }
        if self.received.is_empty() {
            self.reorder_deadline = None;
        }
    }

    fn free_window(&self) -> usize {
        WINDOW.saturating_sub(self.received.len() + self.queued_data)
    }

    fn legacy_header(&self, extra: u16) -> Vec<u8> {
        let mut runs = Vec::<u8>::new();
        for offset in 0..WINDOW {
            let Some(sequence) = self.receive_highest.checked_sub(offset as u64) else {
                break;
            };
            let state = if self.recovery.contains(sequence) {
                0
            } else {
                0xc0
            };
            if let Some(last) = runs.last_mut()
                && *last & 0xc0 == state
                && *last & 63 < 63
            {
                *last += 1;
            } else {
                runs.push(state);
            }
            if sequence <= self.ack_floor {
                break;
            }
        }
        runs.reverse();
        let mut data = Vec::with_capacity(8 + (runs.len() + 2).next_multiple_of(4));
        data.extend_from_slice(&(self.receive_highest as u32).to_be_bytes());
        data.extend_from_slice(&(self.free_window() as u16).to_be_bytes());
        let missing = self.receive_highest >= self.receive_next + 3;
        data.extend_from_slice(
            &(wire::ACK
                | extra
                | if missing { wire::CN } else { 0 }
                | if self.congestion_pending {
                    wire::CWR
                } else {
                    0
                })
            .to_be_bytes(),
        );
        data.extend_from_slice(&(runs.len() as u16).to_be_bytes());
        data.extend_from_slice(&runs);
        data.resize(data.len().next_multiple_of(4), 0);
        data
    }

    fn append_aoa(&self, packet: &mut Vec<u8>) {
        let first = self
            .inflight
            .keys()
            .next()
            .copied()
            .unwrap_or(self.next_source);
        packet.extend_from_slice(&(first.saturating_sub(1) as u32).to_be_bytes());
    }

    fn modern_header(&self, coded: Option<u64>, now_ms: u64) -> Vec<u8> {
        let have_ack = !self.code_received.is_empty();
        let cumulative = have_ack
            && self.code_cumulative_safe
            && self.code_ack_override.is_none()
            && self.code_floor > self.code_highest
            && self.code_received.contains_key(&(self.code_floor - 1));
        let flags = (if cumulative {
            1u16
        } else if have_ack {
            8
        } else {
            0x200
        }) | 0x10
            | if coded.is_some() { 4 } else { 0 };
        let window = self.free_window().max(1).ilog2().min(8) as u16;
        let mut bytes = ((window << 12) | flags).to_le_bytes().to_vec();
        if cumulative {
            let received = self.code_received[&(self.code_floor - 1)];
            bytes.extend_from_slice(&((self.code_floor - 1) as u16).to_le_bytes());
            let timestamp = received.wrapping_mul(250) as u32;
            bytes.extend_from_slice(&timestamp.to_le_bytes()[..3]);
            bytes.extend_from_slice(&[now_ms.saturating_sub(received).min(254) as u8, 0]);
        }
        let oldest = self
            .inflight
            .values()
            .filter_map(|packet| packet.coded)
            .min()
            .unwrap_or(self.next_coded);
        bytes.extend_from_slice(&(oldest as u16).to_le_bytes());
        if let Some(coded) = coded {
            bytes.extend_from_slice(&(coded as u16).to_le_bytes());
        }
        if have_ack && !cumulative {
            let (&first, _) = self
                .code_received
                .first_key_value()
                .expect("nonempty coded receipts");
            let (&last, _) = self
                .code_received
                .last_key_value()
                .expect("nonempty coded receipts");
            let mut base = self.code_ack_override.unwrap_or_else(|| {
                if self.code_cumulative_safe {
                    self.code_floor.min(last)
                } else {
                    first
                }
            });
            if self.code_ack_override.is_none() {
                base = base.max(last.saturating_sub(WINDOW as u64 - 1));
            }
            let count = (last.saturating_sub(base) + 1).min(WINDOW as u64) as usize;
            bytes.extend_from_slice(&(base as u16).to_le_bytes());
            bytes.push(count.div_ceil(7) as u8);
            for start in (0..count).step_by(7) {
                let mut value = 0;
                for offset in 0..7 {
                    if self
                        .code_received
                        .contains_key(&(base + (start + offset) as u64))
                    {
                        value |= 1 << offset;
                    }
                }
                bytes.push(value);
            }
        }
        bytes
    }

    fn source_packet(&mut self, source: u64, bytes: &[u8], now_ms: u64) -> Vec<u8> {
        let coded = self.next_coded;
        self.next_coded += 1;
        if let Some(sent) = self.inflight.get_mut(&source) {
            // AOA retires earlier coded sequence numbers after retransmission.
            // A later cumulative ACK may cover those retired numbers without
            // proving receipt of the retransmitted channel data.
            sent.coded = Some(coded);
        }
        let packet = if self.version == Some(Version::V3) {
            let mut data = self.modern_header(Some(coded), now_ms);
            data.extend_from_slice(&(source as u16).to_le_bytes());
            data.extend_from_slice(bytes);
            wire::wrap_modern(&data)
        } else {
            let mut data = self.legacy_header(wire::DATA | wire::AOA);
            self.append_aoa(&mut data);
            data.extend_from_slice(&(coded as u32).to_be_bytes());
            data.extend_from_slice(&(source as u32).to_be_bytes());
            data.extend_from_slice(bytes);
            data
        };
        self.ack_pending = false;
        self.code_ack_override = None;
        self.congestion_pending = false;
        self.last_send = now_ms;
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn established(version: Version) -> Connection {
        let mut connection = Connection::new(
            Config {
                initial_sequence: 42,
                mode: Mode::Reliable,
                cookie_hash: Some([7; 32]),
            },
            0,
        );
        assert_eq!(connection.poll_transmit(0).unwrap().len(), 1232);
        let version_bytes: [u8; 2] = match version {
            Version::V1 => [0, 1],
            Version::V2 => [0, 2],
            Version::V3 => [1, 1],
        };
        let mut ack = vec![
            0, 0, 0, 42, 1, 0, 0x10, 5, 0, 0, 0, 100, 4, 0xd0, 4, 0xd0, 0, 1,
        ];
        ack.extend_from_slice(&version_bytes);
        connection.receive(&ack, 10).unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Connected));
        if version == Version::V3 {
            assert!(connection.poll_transmit(10).is_none());
        } else {
            assert_eq!(
                connection.poll_transmit(10).unwrap(),
                [0, 0, 0, 100, 1, 0, 0, 4, 0, 1, 0, 0]
            );
        }
        connection
    }
    fn source(sequence: u32, byte: u8) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 42, 1, 0, 0, 8];
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.push(byte);
        bytes
    }

    #[test]
    fn reliable_legacy_orders_and_deduplicates_data() {
        for version in [Version::V1, Version::V2] {
            let mut connection = established(version);
            connection.receive(&source(102, b'B'), 20).unwrap();
            assert!(connection.poll_event().is_none());
            connection.receive(&source(101, b'A'), 21).unwrap();
            assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
            assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
            connection.receive(&source(101, b'A'), 22).unwrap();
            assert!(connection.poll_event().is_none());
        }
    }

    #[test]
    fn lossy_orders_reordered_data_then_skips_expired_gaps() {
        let mut connection = established(Version::V2);
        connection.config.mode = Mode::Lossy;
        connection.receive(&source(102, b'B'), 20).unwrap();
        assert!(connection.poll_event().is_none());
        assert_eq!(connection.next_deadline(), 70);
        connection.receive(&source(101, b'A'), 21).unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
        assert!(connection.reorder_deadline.is_none());
        connection.receive(&source(105, b'E'), 30).unwrap();
        connection.receive(&source(104, b'D'), 31).unwrap();
        assert!(connection.poll_event().is_none());
        connection.poll_transmit(80);
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'D'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'E'])));
        connection.receive(&source(103, b'C'), 81).unwrap();
        assert!(connection.poll_event().is_none());
        assert!(connection.reorder_deadline.is_none());
    }

    #[test]
    fn lossy_right_edge_advances_without_waiting_for_missing_packets() {
        let mut connection = established(Version::V2);
        connection.config.mode = Mode::Lossy;
        connection.receive(&source(102, b'B'), 20).unwrap();
        connection.receive(&source(356, b'Z'), 21).unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'Z'])));
        assert_eq!(connection.receive_next, 357);
        assert!(connection.reorder_deadline.is_none());
    }

    #[test]
    fn reliable_udp2_orders_independent_channel_and_coded_sequences() {
        let mut connection = established(Version::V3);
        // Prefix at index 7; layout DATA, logWindow=8, coded, channel, byte.
        connection
            .receive(&[b'B', 4, 0x80, 102, 0, 2, 0, 0xe0], 20)
            .unwrap();
        assert!(connection.poll_event().is_none());
        connection
            .receive(&[b'A', 4, 0x80, 101, 0, 1, 0, 0xe0], 21)
            .unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
    }

    #[test]
    fn windows_udp2_uses_independent_coded_base_and_noack_flag() {
        let mut connection = Connection::new(
            Config {
                initial_sequence: 42,
                mode: Mode::Reliable,
                cookie_hash: Some([7; 32]),
            },
            0,
        );
        connection.poll_transmit(0).unwrap();
        // The legacy server ISN is unrelated to its first UDP2 coded sequence.
        connection
            .receive(
                &[
                    0, 0, 0, 42, 1, 0, 0x10, 5, 0xfe, 0xdc, 0xba, 0x98, 4, 0xd0, 4, 0xd0, 0, 1, 1,
                    1,
                ],
                10,
            )
            .unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Connected));
        assert!(connection.poll_transmit(10).is_none());
        // Windows DATA|AOA|NOACK; AOA=0, coded=1, channel=2 arrives first.
        connection
            .receive(&[2, 0x14, 0x82, 0, 0, 1, 0, 0xe0, 0, b'B'], 20)
            .unwrap();
        assert!(connection.poll_event().is_none());
        let ack = connection.poll_transmit(20).unwrap();
        let ack = wire::modern(&ack, 65537, 1, 43).unwrap();
        assert_eq!(ack.cumulative, None);
        assert_eq!(ack.acknowledgements, [(65537, 65537)]);
        // The delayed first DATA fills channel 1 and coded 0 independently.
        connection
            .receive(&[1, 0x14, 0x82, 0, 0, 0, 0, 0xe0, 0, b'A'], 21)
            .unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
        let ack = connection.poll_transmit(24).unwrap();
        assert_eq!(
            wire::modern(&ack, 65537, 1, 43).unwrap().cumulative,
            Some(65537)
        );
        // receivedTS is arrival time (20 ms), while sendAckTimeGap is 4 ms.
        assert_eq!(&ack[5..7], &(20u32 * 250).to_le_bytes()[..2]);
        assert_eq!(ack[8], 4);
    }

    #[test]
    fn udp2_aoa_cannot_discard_the_channel_bytes_in_its_own_packet() {
        let mut connection = established(Version::V3);
        // Windows may announce AOA=1 while this DATA still uses coded=0.
        connection
            .receive(&[1, 0x14, 0x82, 1, 0, 0, 0, 0xe0, 0, b'A'], 20)
            .unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
        let ack = connection.poll_transmit(20).unwrap();
        let ack = wire::modern(&ack, 65536, 1, 43).unwrap();
        assert_eq!(ack.cumulative, None);
        assert_eq!(ack.acknowledgements, [(65536, 65536)]);
    }

    #[test]
    fn legacy_ack_encoding_starts_with_the_oldest_run() {
        let mut connection = established(Version::V2);
        for sequence in [101, 104, 105] {
            connection.receive(&source(sequence, b'A'), 20).unwrap();
        }
        connection.ack_floor = 101;
        let ack = connection.legacy_header(0);
        assert_eq!(&ack[8..], &[0, 3, 0, 0xc1, 1, 0, 0, 0]);
    }

    #[test]
    fn udp2_zero_channel_sequence_cannot_extend_lifetime() {
        let mut connection = established(Version::V3);
        let packet = [b'A', 4, 0x80, 101, 0, 0, 0, 0xe0];
        assert!(connection.receive(&packet, 15_000).is_err());
        assert_eq!(connection.last_receive, 10);
        assert!(!connection.code_base_known);
        assert!(connection.poll_transmit(16_010).is_none());
        assert_eq!(connection.poll_event(), Some(Event::Closed));
    }

    #[test]
    fn windows_udp2_channel_sequence_skips_zero_on_wrap() {
        let mut connection = established(Version::V3);
        connection.receive_next = 65535;
        connection
            .receive(&[b'B', 4, 0x80, 102, 0, 1, 0, 0xe0], 20)
            .unwrap();
        connection
            .receive(&[b'A', 4, 0x80, 101, 0, 255, 255, 0xe0], 21)
            .unwrap();
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'A'])));
        assert_eq!(connection.poll_event(), Some(Event::Data(vec![b'B'])));
        assert_eq!(connection.receive_next, 65538);
        connection.next_source = 65535;
        for expected in [65535, 65537] {
            connection.send(b"data").unwrap();
            let data = connection.poll_transmit(30).unwrap();
            let source = wire::modern(&data, 102, expected, 43)
                .unwrap()
                .source
                .unwrap();
            assert_eq!(source.source, expected);
        }
    }

    #[test]
    fn lost_source_retransmits_with_new_coded_sequence() {
        let mut connection = established(Version::V2);
        connection.send(b"hello").unwrap();
        let first = connection.poll_transmit(20).unwrap();
        assert!(connection.poll_transmit(21).is_none());
        let again = connection.poll_transmit(321).unwrap();
        let first = wire::legacy(&first, 100, 43, 43).unwrap().source.unwrap();
        let again = wire::legacy(&again, 100, 43, 44).unwrap().source.unwrap();
        assert_eq!(first.source, again.source);
        assert_eq!(again.coded, first.coded + 1);
        assert_eq!(again.bytes, b"hello");
    }

    #[test]
    fn udp2_retired_packet_ack_cannot_discard_unreceived_retransmission() {
        let mut connection = established(Version::V3);
        connection.send(b"first").unwrap();
        connection.poll_transmit(20).unwrap(); // coded 43
        connection.send(b"second").unwrap();
        connection.poll_transmit(21).unwrap(); // coded 44
        connection.poll_transmit(121).unwrap(); // retransmit first as coded 45
        assert_eq!(connection.inflight[&1].coded, Some(45));
        // After AOA has retired coded 43, ACK 44 must acknowledge only second.
        let ack = wire::wrap_modern(&[1, 0x80, 44, 0, 0, 0, 0, 0, 0]);
        connection.receive(&ack, 122).unwrap();
        assert!(connection.inflight.contains_key(&1));
        assert!(!connection.inflight.contains_key(&2));
    }

    #[test]
    fn send_capacity_is_atomic_and_malformed_packets_do_not_extend_lifetime() {
        let mut connection = established(Version::V3);
        assert!(
            connection
                .send(&vec![0; (WINDOW + 1) * connection.max_payload()])
                .is_err()
        );
        assert!(connection.pending.is_empty());
        assert!(connection.receive(&[0; 7], 15_000).is_err());
        assert!(connection.poll_transmit(16_010).is_none());
        assert_eq!(connection.poll_event(), Some(Event::Closed));
    }
}
