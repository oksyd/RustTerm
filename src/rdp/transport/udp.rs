//! Authenticated RDP multitransport over a bounded reliable UDP byte stream.

use super::super::{RdpError, Result, tls};
mod lossy;
use rustrdp::multitransport::{self, Request, TransportProtocol, TunnelPdu};
use rustrdp::udp::{Config, Connection, Event, Mode};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    net::UdpSocket,
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, sleep_until, timeout},
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::bytes::{Buf, Bytes, BytesMut};

const STREAM_CAPACITY: usize = 256 * 1024;
const RECEIVE_CAPACITY: usize = 512 * 1024;
const MAX_TUNNEL_PDU: usize = 65535 + 255;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The certificate fingerprint comes from the already authenticated TCP stream.
pub(in crate::rdp) async fn connect(
    peer: SocketAddr,
    server_name: String,
    certificate_fingerprint: String,
    request: Request,
) -> Result<Tunnel> {
    timeout(CONNECT_TIMEOUT, async {
        if request.protocol == TransportProtocol::Lossy {
            return lossy::connect(peer, server_name, certificate_fingerprint, request)
                .await
                .map(Tunnel::Lossy);
        }
        let stream = ReliableStream::connect(peer, request.cookie).await?;
        let config = tls::config(Some(certificate_fingerprint), Arc::new(Mutex::new(None)))?;
        let name =
            rustls::pki_types::ServerName::try_from(server_name).map_err(|_| RdpError::Tls)?;
        let mut secured = TlsConnector::from(config)
            .connect(name, stream)
            .await
            .map_err(|_| RdpError::Tls)?;
        secured
            .write_all(&request.create_pdu())
            .await
            .map_err(|_| RdpError::Connection)?;
        secured.flush().await.map_err(|_| RdpError::Connection)?;
        let mut tunnel = Tunnel::Reliable {
            stream: Box::new(secured),
            buffered: BytesMut::new(),
        };
        let response = tunnel.read_frame().await?;
        match multitransport::decode_pdu(&response)? {
            TunnelPdu::Created(status) if status & 0x8000_0000 == 0 => Ok(tunnel),
            _ => Err(RdpError::Protocol("UDP tunnel creation response")),
        }
    })
    .await
    .map_err(|_| RdpError::Timeout)?
}

pub(in crate::rdp) enum Tunnel {
    Reliable {
        stream: Box<TlsStream<ReliableStream>>,
        buffered: BytesMut,
    },
    Lossy(lossy::Tunnel),
}

impl Tunnel {
    pub fn split(self) -> (TunnelReader, TunnelWriter) {
        match self {
            Self::Reliable { stream, buffered } => {
                let (read, write) = tokio::io::split(stream);
                (
                    TunnelReader::Reliable {
                        stream: read,
                        buffered,
                    },
                    TunnelWriter::Reliable(write),
                )
            }
            Self::Lossy(tunnel) => (
                TunnelReader::Lossy(tunnel.reader),
                TunnelWriter::Lossy(tunnel.writer),
            ),
        }
    }
    async fn read_frame(&mut self) -> Result<Bytes> {
        match self {
            Self::Reliable { stream, buffered } => read_frame(stream, buffered).await,
            Self::Lossy(tunnel) => tunnel.reader.read_frame().await,
        }
    }
}

pub(in crate::rdp) enum TunnelReader {
    Reliable {
        stream: tokio::io::ReadHalf<Box<TlsStream<ReliableStream>>>,
        buffered: BytesMut,
    },
    Lossy(lossy::Reader),
}
impl TunnelReader {
    pub async fn read_frame(&mut self) -> Result<Bytes> {
        match self {
            Self::Reliable { stream, buffered } => read_frame(stream, buffered).await,
            Self::Lossy(reader) => reader.read_frame().await,
        }
    }
}

pub(in crate::rdp) enum TunnelWriter {
    Reliable(tokio::io::WriteHalf<Box<TlsStream<ReliableStream>>>),
    Lossy(lossy::Writer),
}
impl TunnelWriter {
    pub async fn send_frame(&mut self, packet: &[u8]) -> Result<()> {
        match self {
            Self::Reliable(stream) => {
                stream
                    .write_all(packet)
                    .await
                    .map_err(|_| RdpError::Connection)?;
                stream.flush().await.map_err(|_| RdpError::Connection)
            }
            Self::Lossy(writer) => writer.send_frame(packet.to_vec()).await,
        }
    }

    pub async fn send_batch(&mut self, payloads: &[Vec<u8>]) -> Result<()> {
        match self {
            Self::Reliable(stream) => {
                for payload in payloads {
                    stream
                        .write_all(&multitransport::data_pdu(payload)?)
                        .await
                        .map_err(|_| RdpError::Connection)?;
                }
                stream.flush().await.map_err(|_| RdpError::Connection)
            }
            Self::Lossy(writer) => {
                for payload in payloads {
                    writer.send(payload).await?;
                }
                Ok(())
            }
        }
    }
}

fn random_sequence() -> u32 {
    let id = uuid::Uuid::new_v4();
    let bytes = id.as_bytes();
    u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffered: &mut BytesMut,
) -> Result<Bytes> {
    loop {
        if let Some(length) = multitransport::frame_size(buffered)?
            && buffered.len() >= length
        {
            return Ok(buffered.split_to(length).freeze());
        }
        let available = MAX_TUNNEL_PDU.saturating_sub(buffered.len());
        if available == 0 {
            return Err(RdpError::Limit("UDP tunnel receive buffer"));
        }
        let mut data = [0; 16384];
        let limit = available.min(data.len());
        let count = stream
            .read(&mut data[..limit])
            .await
            .map_err(|_| RdpError::Connection)?;
        if count == 0 {
            return Err(RdpError::Disconnected);
        }
        buffered.extend_from_slice(&data[..count]);
    }
}

pub(in crate::rdp) struct ReliableStream {
    stream: DuplexStream,
    driver: JoinHandle<()>,
}

impl ReliableStream {
    async fn connect(peer: SocketAddr, cookie: [u8; 16]) -> Result<Self> {
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
        let config = Config {
            initial_sequence: random_sequence(),
            mode: Mode::Reliable,
            cookie_hash: Some(Sha256::digest(cookie).into()),
        };
        let connection = Connection::new(config, 0);
        let (stream, network) = tokio::io::duplex(STREAM_CAPACITY);
        let (ready, connected) = oneshot::channel();
        let driver = tokio::spawn(async move {
            let _ = drive(socket, network, connection, ready).await;
        });
        let result = Self { stream, driver };
        connected.await.map_err(|_| RdpError::Connection)?;
        Ok(result)
    }
}

impl Drop for ReliableStream {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl AsyncRead for ReliableStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for ReliableStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

async fn drive(
    socket: UdpSocket,
    stream: DuplexStream,
    mut connection: Connection,
    ready: oneshot::Sender<()>,
) -> io::Result<()> {
    let started = Instant::now();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut ready = Some(ready);
    let mut pending_send: Option<Vec<u8>> = None;
    let mut received = VecDeque::<Bytes>::new();
    let mut buffered = 0usize;
    let mut network = [0; 65536];
    let mut outgoing = [0; 16384];
    loop {
        let now = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        if let Some(data) = pending_send.take() {
            match connection.send(&data) {
                Ok(()) => {}
                Err(rustrdp::Error::Limit(_)) => pending_send = Some(data),
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        // Bound each turn so a busy graphics stream cannot monopolize the runtime.
        let mut burst = 0;
        while burst < 32 {
            let Some(packet) = connection.poll_transmit(now) else {
                break;
            };
            socket.send(&packet).await?;
            burst += 1;
        }
        let mut released_data = false;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::Connected => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                Event::Data(data) => {
                    released_data = true;
                    buffered = buffered
                        .checked_add(data.len())
                        .ok_or_else(|| io::Error::other("UDP receive limit"))?;
                    if buffered > RECEIVE_CAPACITY {
                        return Err(io::Error::other("UDP receive limit"));
                    }
                    if !data.is_empty() {
                        received.push_back(data.into());
                    }
                }
                Event::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "RDP UDP peer closed",
                    ));
                }
            }
        }
        if burst == 32 {
            tokio::task::yield_now().await;
            continue;
        }
        if released_data {
            // Draining data opens the receive window; publish its ACK before
            // sleeping, or a peer that saw a zero window cannot resume sending.
            continue;
        }
        let deadline = started + Duration::from_millis(connection.next_deadline());
        let front = received.front().map_or(&[][..], |bytes| bytes.as_ref());
        tokio::select! {
            result = socket.recv(&mut network), if buffered <= RECEIVE_CAPACITY - 65536 => {
                let count = result?;
                let now = started.elapsed().as_millis() as u64;
                // Malformed datagrams cannot be authenticated by UDP. Ignore them
                // without refreshing state beyond what the decoder accepted.
                let _ = connection.receive(&network[..count], now);
            }
            result = reader.read(&mut outgoing), if pending_send.is_none() && connection.is_established() => {
                let count = result?;
                if count == 0 { connection.close(); return Ok(()); }
                pending_send = Some(outgoing[..count].to_vec());
            }
            result = writer.write(front), if !front.is_empty() => {
                let count = result?;
                if count == 0 { return Err(io::ErrorKind::WriteZero.into()); }
                buffered -= count;
                let front = received.front_mut().ok_or_else(|| io::Error::other("UDP receive queue"))?;
                front.advance(count);
                if front.is_empty() { received.pop_front(); }
            }
            _ = sleep_until(deadline) => {
                // poll_transmit advances the protocol deadlines at the top of the loop.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reliable_socket_orders_duplicates_and_ignores_bad_datagrams() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = server.local_addr().unwrap();
        let handshake = tokio::spawn(async move {
            let mut packet = [0; 4096];
            let (count, client) = server.recv_from(&mut packet).await.unwrap();
            assert_eq!(count, 1232);
            let initial = u32::from_be_bytes(packet[8..12].try_into().unwrap());
            server.connect(client).await.unwrap();
            let mut ack = initial.to_be_bytes().to_vec();
            ack.extend_from_slice(&[1, 0, 0x10, 5, 0, 0, 0, 100, 4, 0xd0, 4, 0xd0, 0, 1, 0, 2]);
            server.send(&ack).await.unwrap();
            server.recv(&mut packet).await.unwrap();
            for (sequence, byte) in [(102u32, b'B'), (102, b'B'), (101, b'A')] {
                let mut data = initial.to_be_bytes().to_vec();
                data.extend_from_slice(&[1, 0, 0, 8]);
                data.extend_from_slice(&sequence.to_be_bytes());
                data.extend_from_slice(&sequence.to_be_bytes());
                data.push(byte);
                server.send(&data).await.unwrap();
            }
            server.send(&[1, 2, 3]).await.unwrap();
            // A well-formed parity packet for an already received source must
            // not close the connection or duplicate data delivered to TLS.
            let mut parity = initial.to_be_bytes().to_vec();
            parity.extend_from_slice(&[
                1, 0, 0, 0x18, 0, 0, 0, 103, 0, 0, 0, 101, 0, 102, 0, 0, 0, 0,
            ]);
            server.send(&parity).await.unwrap();
            server
        });
        let mut stream = timeout(
            Duration::from_secs(2),
            ReliableStream::connect(peer, [9; 16]),
        )
        .await
        .unwrap()
        .unwrap();
        let server = handshake.await.unwrap();
        let mut received = [0; 2];
        timeout(Duration::from_secs(2), stream.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, *b"AB");
        stream.write_all(b"outgoing").await.unwrap();
        let mut packet = [0; 4096];
        timeout(Duration::from_secs(2), async {
            loop {
                let count = server.recv(&mut packet).await.unwrap();
                if packet[..count].ends_with(b"outgoing") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let driver = stream.driver.abort_handle();
        drop(stream);
        tokio::task::yield_now().await;
        assert!(driver.is_finished());
    }

    #[tokio::test]
    async fn tunnel_framing_survives_cancel_and_coalesced_messages() {
        let (mut sender, mut receiver) = tokio::io::duplex(256);
        let mut buffered = BytesMut::new();
        sender.write_all(&[2, 3]).await.unwrap();
        assert!(
            timeout(
                Duration::from_millis(1),
                read_frame(&mut receiver, &mut buffered)
            )
            .await
            .is_err()
        );
        sender
            .write_all(&[0, 4, b'a', b'b', b'c', 2, 1, 0, 4, b'd'])
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut receiver, &mut buffered)
                .await
                .unwrap()
                .as_ref(),
            &[2, 3, 0, 4, b'a', b'b', b'c']
        );
        assert_eq!(
            read_frame(&mut receiver, &mut buffered)
                .await
                .unwrap()
                .as_ref(),
            &[2, 1, 0, 4, b'd']
        );
    }
}
