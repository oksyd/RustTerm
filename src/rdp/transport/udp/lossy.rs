//! DTLS association bound to the main RDP connection's server certificate.

use super::{RdpError, Request, Result, TunnelPdu, multitransport, tls};
use rtc_dtls::{
    config::{ConfigBuilder, ExtendedMasterSecretType},
    crypto_provider,
    endpoint::{Endpoint, EndpointEvent},
};
use rustrdp::udp::{Config, Connection, Event, Mode};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::bytes::{Bytes, BytesMut};

const MAX_PAYLOAD: usize = 768;

pub(in crate::rdp) struct Tunnel {
    pub reader: Reader,
    pub writer: Writer,
}

pub(in crate::rdp) struct Reader {
    incoming: mpsc::Receiver<Result<Bytes>>,
    _task: Arc<Task>,
}
pub(in crate::rdp) struct Writer {
    outgoing: mpsc::Sender<Vec<u8>>,
    _task: Arc<Task>,
}
struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Reader {
    pub async fn read_frame(&mut self) -> Result<Bytes> {
        self.incoming.recv().await.ok_or(RdpError::Disconnected)?
    }
}
impl Writer {
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > MAX_PAYLOAD {
            return Err(RdpError::Limit("lossy tunnel payload"));
        }
        self.send_frame(multitransport::data_pdu(data)?).await
    }

    pub async fn send_frame(&mut self, packet: Vec<u8>) -> Result<()> {
        if packet.len() > MAX_PAYLOAD + 4 {
            return Err(RdpError::Limit("lossy tunnel packet"));
        }
        self.outgoing
            .send(packet)
            .await
            .map_err(|_| RdpError::Disconnected)
    }
}

fn handshake_config(
    peer: SocketAddr,
    name: String,
    fingerprint: String,
) -> Result<Arc<rtc_dtls::config::HandshakeConfig>> {
    let config = ConfigBuilder::default()
        .with_crypto_provider(crypto_provider::default_provider().map_err(|_| RdpError::Tls)?)
        .with_server_name(name)
        .with_mtu(844)
        .with_extended_master_secret(ExtendedMasterSecretType::Request)
        // The authenticated main connection supplies the identity. DTLS still
        // checks handshake signatures and the Finished transcript MAC.
        .with_insecure_skip_verify(true)
        .with_verify_peer_certificate(Some(Arc::new(move |chain, _| {
            if chain
                .first()
                .is_some_and(|cert| tls::fingerprint(cert).eq_ignore_ascii_case(&fingerprint))
            {
                Ok(())
            } else {
                Err(rtc_shared::error::Error::Crypto(
                    "RDP tunnel certificate mismatch".into(),
                ))
            }
        })))
        .build(true, Some(peer))
        .map_err(|_| RdpError::Tls)?;
    Ok(Arc::new(config))
}

pub(super) async fn connect(
    peer: SocketAddr,
    name: String,
    fingerprint: String,
    request: Request,
) -> Result<Tunnel> {
    let local: SocketAddr = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .map_err(|_| RdpError::Connection)?;
    let socket = UdpSocket::bind(local)
        .await
        .map_err(|_| RdpError::Connection)?;
    socket
        .connect(peer)
        .await
        .map_err(|_| RdpError::Connection)?;
    let local = socket.local_addr().map_err(|_| RdpError::Connection)?;
    let config = handshake_config(peer, name, fingerprint)?;
    let endpoint = Endpoint::new(local, rtc_shared::TransportProtocol::UDP, None);
    let (send, receive) = mpsc::channel(64);
    let (incoming, outgoing) = mpsc::channel(64);
    let (ready, completed) = oneshot::channel();
    let task = Arc::new(Task(tokio::spawn(async move {
        if let Err(error) = drive(
            socket,
            request,
            endpoint,
            config,
            receive,
            incoming.clone(),
            ready,
        )
        .await
        {
            let _ = incoming.send(Err(error)).await;
        }
    })));
    let tunnel = Tunnel {
        reader: Reader {
            incoming: outgoing,
            _task: task.clone(),
        },
        writer: Writer {
            outgoing: send,
            _task: task,
        },
    };
    completed.await.map_err(|_| RdpError::Connection)??;
    Ok(tunnel)
}

async fn drive(
    socket: UdpSocket,
    request: Request,
    mut dtls: Endpoint,
    config: Arc<rtc_dtls::config::HandshakeConfig>,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
    incoming: mpsc::Sender<Result<Bytes>>,
    ready: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let peer = socket.peer_addr().map_err(|_| RdpError::Connection)?;
    let started = Instant::now();
    let mut udp = Connection::new(
        Config {
            initial_sequence: super::random_sequence(),
            mode: Mode::Lossy,
            cookie_hash: None,
        },
        0,
    );
    let mut secured = false;
    let mut ready = Some(ready);
    let mut created = false;
    let mut network = [0; 4096];
    let mut timer = tokio::time::interval(Duration::from_millis(25));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let now = Instant::now();
        let millis = started.elapsed().as_millis() as u64;
        while let Some(packet) = dtls.poll_transmit() {
            udp.send(&packet.message)?;
        }
        let mut burst = 0;
        while burst < 32 {
            let Some(packet) = udp.poll_transmit(millis) else {
                break;
            };
            socket
                .send(&packet)
                .await
                .map_err(|_| RdpError::Connection)?;
            burst += 1;
        }
        if udp.is_closed() {
            return Err(RdpError::Timeout);
        }
        if udp.is_established() && !secured {
            dtls.connect(now, peer, config.clone(), None)
                .map_err(|_| RdpError::Tls)?;
            secured = true;
            continue;
        }
        let mut received_data = false;
        while let Some(event) = udp.poll_event() {
            let record = match event {
                Event::Data(bytes) => bytes,
                Event::Connected => continue,
                Event::Closed => return Err(RdpError::Disconnected),
            };
            received_data = true;
            let events = dtls
                .read(now, peer, None, BytesMut::from(record.as_slice()))
                .map_err(|_| RdpError::Tls)?;
            for event in events {
                match event {
                    EndpointEvent::HandshakeComplete => {
                        dtls.write(now, peer, &request.create_pdu())
                            .map_err(|_| RdpError::Tls)?;
                    }
                    EndpointEvent::ApplicationData(data) => {
                        match multitransport::decode_pdu(&data)? {
                            TunnelPdu::Created(status) if !created && status & 0x8000_0000 == 0 => {
                                created = true;
                                if let Some(ready) = ready.take() {
                                    let _ = ready.send(Ok(()));
                                }
                            }
                            TunnelPdu::Data { .. } if created => {
                                // A slow consumer must not block DTLS keepalives or
                                // grow memory. Lossy transport permits discarding data.
                                match incoming.try_send(Ok(data.freeze())) {
                                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                                }
                            }
                            _ => return Err(RdpError::Protocol("lossy tunnel response state")),
                        }
                    }
                    EndpointEvent::PeerClosed => return Err(RdpError::Disconnected),
                    _ => {}
                }
            }
        }
        if received_data || burst == 32 {
            tokio::task::yield_now().await;
            continue;
        }
        tokio::select! {
            count = socket.recv(&mut network) => {
                let count = count.map_err(|_| RdpError::Connection)?;
                if udp.receive(&network[..count], started.elapsed().as_millis() as u64).is_err() { continue; }

            }
            data = outgoing.recv(), if created => {
                let Some(data) = data else { return Ok(()); };
                dtls.write(Instant::now(), peer, &data).map_err(|_| RdpError::Tls)?;
            }
            _ = timer.tick() => {
                if dtls.poll_timeout(&peer).is_some_and(|deadline| deadline <= Instant::now()) {
                    dtls.handle_timeout(peer, Instant::now()).map_err(|_| RdpError::Tls)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc_dtls::crypto::Certificate;

    #[test]
    fn dtls_handshake_requires_the_authenticated_main_certificate() {
        let provider = crypto_provider::default_provider().unwrap();
        let certificate =
            Certificate::generate_self_signed(vec!["localhost".into()], provider.crypto()).unwrap();
        let fingerprint = tls::fingerprint(certificate.certificate[0].as_ref());
        let server_config = Arc::new(
            ConfigBuilder::default()
                .with_crypto_provider(provider)
                .with_certificates(vec![certificate])
                .build(false, None)
                .unwrap(),
        );
        let client_address: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let server_address: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        for (pin, expected) in [(fingerprint, true), ("wrong-certificate".into(), false)] {
            let mut client =
                Endpoint::new(client_address, rtc_shared::TransportProtocol::UDP, None);
            let mut server = Endpoint::new(
                server_address,
                rtc_shared::TransportProtocol::UDP,
                Some(server_config.clone()),
            );
            let config = handshake_config(server_address, "localhost".into(), pin).unwrap();
            let now = Instant::now();
            client.connect(now, server_address, config, None).unwrap();
            let mut completed = false;
            let mut rejected = false;
            'handshake: for _ in 0..16 {
                while let Some(packet) = client.poll_transmit() {
                    server
                        .read(now, client_address, None, packet.message)
                        .unwrap();
                }
                while let Some(packet) = server.poll_transmit() {
                    match client.read(now, server_address, None, packet.message) {
                        Ok(events) => {
                            completed |= events
                                .iter()
                                .any(|event| matches!(event, EndpointEvent::HandshakeComplete))
                        }
                        Err(error) => {
                            assert!(
                                error.to_string().contains("certificate mismatch"),
                                "{error}"
                            );
                            rejected = true;
                            break 'handshake;
                        }
                    }
                }
                if completed {
                    break;
                }
            }
            assert_eq!(completed, expected);
            assert_eq!(rejected, !expected);
            if completed {
                client
                    .write(now, server_address, b"authenticated tunnel")
                    .unwrap();
                let mut received = false;
                while let Some(packet) = client.poll_transmit() {
                    for event in server
                        .read(now, client_address, None, packet.message)
                        .unwrap()
                    {
                        if let EndpointEvent::ApplicationData(data) = event {
                            assert_eq!(data.as_ref(), b"authenticated tunnel");
                            received = true;
                        }
                    }
                }
                assert!(received);
            }
        }
    }
}
