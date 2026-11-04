use super::udp;
use crate::rdp::{RdpError, Result};
use rustrdp::multitransport::{
    BandwidthDetection, Request, TransportProtocol, TunnelPdu, decode_pdu,
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tokio_util::{bytes::Bytes, sync::CancellationToken};

const EVENT_CAPACITY: usize = 16;
const WRITE_CAPACITY: usize = 16;
const MAX_BATCH_BYTES: usize = 1024 * 1024;

pub(in crate::rdp) enum Update {
    Connected {
        request_id: u32,
        result: Result<()>,
    },
    Data {
        protocol: TransportProtocol,
        packet: Bytes,
    },
    Closed {
        protocol: TransportProtocol,
    },
}

enum Event {
    Connected {
        request: Request,
        sender: mpsc::Sender<Write>,
    },
    Failed {
        request: Request,
    },
    Data {
        request_id: u32,
        protocol: TransportProtocol,
        packet: Bytes,
    },
    Closed {
        request_id: u32,
        protocol: TransportProtocol,
    },
}

struct Write {
    payloads: Vec<Vec<u8>>,
    cancel: Option<CancellationToken>,
}

struct Connection {
    request_id: u32,
    sender: Option<mpsc::Sender<Write>>,
}

/// Owns optional UDP connections without blocking the main RDP handshake or I/O.
pub(in crate::rdp) struct Sidebands {
    peer: SocketAddr,
    name: String,
    fingerprint: String,
    deadline: Duration,
    connections: BTreeMap<TransportProtocol, Connection>,
    tasks: JoinSet<()>,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
}

impl Sidebands {
    pub fn new(peer: SocketAddr, name: String, fingerprint: String, deadline: Duration) -> Self {
        let (sender, events) = mpsc::channel(EVENT_CAPACITY);
        Self {
            peer,
            name,
            fingerprint,
            deadline,
            connections: BTreeMap::new(),
            tasks: JoinSet::new(),
            events,
            sender,
        }
    }

    pub fn open(
        &mut self,
        request: Request,
        handshake_deadline: Option<tokio::time::Instant>,
    ) -> bool {
        if self.connections.contains_key(&request.protocol) {
            return false;
        }
        while self.tasks.try_join_next().is_some() {}
        self.connections.insert(
            request.protocol,
            Connection {
                request_id: request.id,
                sender: None,
            },
        );
        let peer = self.peer;
        let name = self.name.clone();
        let fingerprint = self.fingerprint.clone();
        let events = self.sender.clone();
        let deadline = self.deadline;
        // Optional transports must fail early enough to report their failure on
        // TCP and let activation finish within the original handshake deadline.
        let connect_budget = handshake_deadline.map_or(deadline, |end| {
            deadline
                .min(end.saturating_duration_since(tokio::time::Instant::now()) / 2)
                .min(Duration::from_secs(3))
        });
        self.tasks.spawn(async move {
            let tunnel = match timeout(
                connect_budget,
                udp::connect(peer, name, fingerprint, request.clone()),
            )
            .await
            {
                Ok(Ok(tunnel)) => tunnel,
                _ => {
                    let _ = events.send(Event::Failed { request }).await;
                    return;
                }
            };
            let (sender, commands) = mpsc::channel(WRITE_CAPACITY);
            if events
                .send(Event::Connected {
                    request: request.clone(),
                    sender,
                })
                .await
                .is_err()
            {
                return;
            }
            let _ = run(tunnel, commands, &events, &request, deadline).await;
            let _ = events
                .send(Event::Closed {
                    request_id: request.id,
                    protocol: request.protocol,
                })
                .await;
        });
        true
    }

    pub async fn next(&mut self) -> Option<Update> {
        loop {
            let event = self.events.recv().await?;
            match event {
                Event::Connected { request, sender } => {
                    if let Some(connection) = self.connections.get_mut(&request.protocol)
                        && connection.request_id == request.id
                    {
                        connection.sender = Some(sender);
                        return Some(Update::Connected {
                            request_id: request.id,
                            result: Ok(()),
                        });
                    }
                }
                Event::Failed { request } => {
                    if self.matches(request.protocol, request.id) {
                        self.connections.remove(&request.protocol);
                        return Some(Update::Connected {
                            request_id: request.id,
                            result: Err(RdpError::Connection),
                        });
                    }
                }
                Event::Data {
                    request_id,
                    protocol,
                    packet,
                } => {
                    if self.matches(protocol, request_id) {
                        return Some(Update::Data { protocol, packet });
                    }
                }
                Event::Closed {
                    request_id,
                    protocol,
                } => {
                    if self.matches(protocol, request_id) {
                        self.connections.remove(&protocol);
                        return Some(Update::Closed { protocol });
                    }
                }
            }
        }
    }

    pub async fn send(
        &self,
        protocol: TransportProtocol,
        payloads: Vec<Vec<u8>>,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        if payloads
            .iter()
            .try_fold(0usize, |bytes, payload| bytes.checked_add(payload.len()))
            .is_none_or(|bytes| bytes > MAX_BATCH_BYTES)
        {
            return Err(RdpError::Limit("UDP tunnel write batch"));
        }
        let sender = self
            .connections
            .get(&protocol)
            .and_then(|connection| connection.sender.as_ref())
            .ok_or(RdpError::Connection)?;
        timeout(self.deadline, sender.send(Write { payloads, cancel }))
            .await
            .map_err(|_| RdpError::Timeout)?
            .map_err(|_| RdpError::Connection)
    }

    pub fn connected(&self) -> usize {
        self.connections
            .values()
            .filter(|connection| connection.sender.is_some())
            .count()
    }

    fn matches(&self, protocol: TransportProtocol, request_id: u32) -> bool {
        self.connections
            .get(&protocol)
            .is_some_and(|connection| connection.request_id == request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn optional_udp_reports_failure_before_the_handshake_expires() {
        // An open UDP socket that never replies simulates a firewall silently
        // dropping the handshake, without ICMP producing an immediate failure.
        let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut tunnels = Sidebands::new(
            blackhole.local_addr().unwrap(),
            "localhost".into(),
            "unused".into(),
            Duration::from_secs(20),
        );
        let request = Request {
            id: 19,
            protocol: TransportProtocol::Reliable,
            cookie: [3; 16],
        };
        let end = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(tunnels.open(request.clone(), Some(end)));
        let result = tokio::time::timeout_at(end, async {
            let Some(Update::Connected {
                request_id,
                result: Err(_),
            }) = tunnels.next().await
            else {
                panic!("missing optional tunnel failure");
            };
            assert_eq!(request_id, request.id);
            assert_eq!(tunnels.connected(), 0);
            // The caller still has time to finish the TCP failure response.
            tokio::time::sleep(Duration::from_millis(10)).await;
            request.response(0x8000_4004)
        })
        .await
        .unwrap();
        assert_eq!(&result[8..], &0x8000_4004u32.to_le_bytes());
    }
}

async fn run(
    tunnel: udp::Tunnel,
    mut commands: mpsc::Receiver<Write>,
    events: &mpsc::Sender<Event>,
    request: &Request,
    deadline: Duration,
) -> Result<()> {
    let (mut reader, mut writer) = tunnel.split();
    let (controls, mut responses) = mpsc::channel::<Vec<u8>>(4);
    let receive = receive(&mut reader, events, request, controls);
    let send = async {
        loop {
            tokio::select! {
                response = responses.recv() => {
                    let Some(packet) = response else { return Ok::<(), RdpError>(()); };
                    timeout(deadline, writer.send_frame(&packet))
                        .await.map_err(|_| RdpError::Timeout)??;
                }
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    if command.cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                        continue;
                    }
                    // Do not cancel between fragments of the same dynamic-channel PDU.
                    timeout(deadline, writer.send_batch(&command.payloads))
                        .await.map_err(|_| RdpError::Timeout)??;
                }
            }
        }
    };
    tokio::select! {
        result = receive => result,
        result = send => result,
    }
}

async fn receive(
    reader: &mut udp::TunnelReader,
    events: &mpsc::Sender<Event>,
    request: &Request,
    controls: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let started = Instant::now();
    let mut detection = BandwidthDetection::new(request.protocol);
    loop {
        let packet = reader.read_frame().await?;
        let TunnelPdu::Data {
            subheaders,
            payload,
        } = decode_pdu(&packet)?
        else {
            return Err(RdpError::Protocol("unexpected UDP tunnel control message"));
        };
        for response in detection.receive(
            subheaders,
            payload.len(),
            started.elapsed().as_millis() as u64,
        )? {
            controls
                .send(response)
                .await
                .map_err(|_| RdpError::Disconnected)?;
        }
        if payload.is_empty() {
            continue;
        }
        events
            .send(Event::Data {
                request_id: request.id,
                protocol: request.protocol,
                packet,
            })
            .await
            .map_err(|_| RdpError::Disconnected)?;
    }
}
