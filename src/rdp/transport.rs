use super::{RdpError, Result};
pub(super) mod sidebands;
pub(super) mod udp;
use rustrdp::wire;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::{
    bytes::{BufMut, Bytes, BytesMut},
    sync::CancellationToken,
};
use zeroize::Zeroize;

const MAX_PDU: usize = 4 * 1024 * 1024;
const MAX_CREDSSP: usize = 65536;
const READ_CHUNK: usize = 16 * 1024;
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
const MAX_QUEUED_PACKETS: usize = 32768;

/// Cancellation-safe framing: a cancelled read retains its partial message.
pub(super) struct Framed<S> {
    stream: S,
    buffered: BytesMut,
}

impl<S> Framed<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buffered: BytesMut::new(),
        }
    }

    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    pub fn into_stream(self) -> Result<S> {
        if self.buffered.is_empty() {
            Ok(self.stream)
        } else {
            Err(RdpError::Protocol("unconsumed transport input"))
        }
    }
}

impl<S: AsyncRead + AsyncWrite> Framed<S> {
    pub fn split(
        self,
    ) -> (
        Framed<tokio::io::ReadHalf<S>>,
        Framed<tokio::io::WriteHalf<S>>,
    ) {
        let (read, write) = tokio::io::split(self.stream);
        (
            Framed {
                stream: read,
                buffered: self.buffered,
            },
            Framed::new(write),
        )
    }
}

impl<S: AsyncRead + Unpin> Framed<S> {
    pub async fn read_pdu(&mut self) -> Result<Bytes> {
        self.read_frame(|bytes| Ok(wire::frame_size(bytes, MAX_PDU)?.map(|(_, size)| size)))
            .await
            .map(BytesMut::freeze)
    }

    pub async fn read_credssp(&mut self) -> Result<Vec<u8>> {
        let mut frame = self
            .read_frame(|bytes| Ok(wire::credssp_size(bytes, MAX_CREDSSP)?))
            .await?;
        // The caller owns and zeroizes the authentication message. Do not leave
        // another copy in the allocation shared with the following wire frame.
        let bytes = frame.to_vec();
        frame.as_mut().zeroize();
        Ok(bytes)
    }

    pub async fn read_early_auth(&mut self) -> Result<Vec<u8>> {
        self.read_frame(|_| Ok(Some(4)))
            .await
            .map(|frame| frame.to_vec())
    }

    async fn read_frame(
        &mut self,
        length: impl Fn(&[u8]) -> Result<Option<usize>>,
    ) -> Result<BytesMut> {
        loop {
            if let Some(size) = length(&self.buffered)?
                && self.buffered.len() >= size
            {
                return Ok(self.buffered.split_to(size));
            }
            let remaining = MAX_PDU
                .checked_sub(self.buffered.len())
                .filter(|n| *n > 0)
                .ok_or(RdpError::Limit("transport receive buffer"))?;
            let amount = remaining.min(READ_CHUNK);
            self.buffered.reserve(amount);
            let mut buffer = (&mut self.buffered).limit(amount);
            let count = self
                .stream
                .read_buf(&mut buffer)
                .await
                .map_err(|_| RdpError::Connection)?;
            if count == 0 {
                return Err(RdpError::Connection);
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> Framed<S> {
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > MAX_PDU {
            return Err(RdpError::Limit("outgoing transport packet"));
        }
        self.stream
            .write_all(data)
            .await
            .map_err(|_| RdpError::Connection)?;
        // TLS may accept plaintext while encrypted records remain buffered.
        self.stream.flush().await.map_err(|_| RdpError::Connection)
    }
}

/// A single writer preserves each channel's fragment order while allowing input
/// and audio control to make progress between bulk clipboard fragments.
pub(super) struct Writer {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
    duration: Duration,
    pub errors: mpsc::Receiver<RdpError>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Notify,
    available: Notify,
}

struct Queue {
    channels: BTreeMap<u16, VecDeque<Queued>>,
    ready: VecDeque<u16>,
    // Include the currently written packet, not just packets waiting in memory.
    bytes: usize,
    packets: usize,
    byte_limit: usize,
    packet_limit: usize,
    failure: Option<RdpError>,
}

struct Queued {
    packet: Vec<u8>,
    cancel: Option<CancellationToken>,
    receipt: Option<oneshot::Sender<Result<()>>>,
}

impl Writer {
    pub fn new<W: AsyncWrite + Unpin + Send + 'static>(
        write: Framed<W>,
        duration: Duration,
    ) -> Self {
        Self::with_limits(write, duration, MAX_QUEUED_BYTES, MAX_QUEUED_PACKETS)
    }

    fn with_limits<W: AsyncWrite + Unpin + Send + 'static>(
        mut write: Framed<W>,
        duration: Duration,
        byte_limit: usize,
        packet_limit: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                channels: BTreeMap::new(),
                ready: VecDeque::new(),
                bytes: 0,
                packets: 0,
                byte_limit,
                packet_limit,
                failure: None,
            }),
            ready: Notify::new(),
            available: Notify::new(),
        });
        let worker = shared.clone();
        let (errors_tx, errors) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                let next = worker
                    .queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pop();
                let Some(queued) = next else {
                    worker.ready.notified().await;
                    continue;
                };
                let length = queued.packet.len();
                if queued
                    .cancel
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                {
                    worker
                        .queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .finished(length);
                    worker.available.notify_waiters();
                    if let Some(receipt) = queued.receipt {
                        let _ = receipt.send(Err(RdpError::ClipboardUnavailable));
                    }
                    continue;
                }
                // Cancellation is checked only before beginning a packet. Once
                // bytes have been written, finish it or close the transport.
                let result = timeout(duration, write.write(&queued.packet))
                    .await
                    .map_err(|_| RdpError::Timeout)
                    .and_then(|result| result);
                match result {
                    Ok(()) => {
                        worker
                            .queue
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .finished(length);
                        worker.available.notify_waiters();
                        if let Some(receipt) = queued.receipt {
                            let _ = receipt.send(Ok(()));
                        }
                    }
                    Err(error) => {
                        let pending = worker
                            .queue
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .fail(error.clone());
                        worker.available.notify_waiters();
                        if let Some(receipt) = queued.receipt {
                            let _ = receipt.send(Err(error.clone()));
                        }
                        reject_receipts(pending, error.clone());
                        let _ = errors_tx.try_send(error);
                        return;
                    }
                }
            }
        });
        Self {
            shared,
            task,
            duration,
            errors,
        }
    }

    /// `cancel` applies to a complete packet or a complete bundled channel message.
    /// Never attach it to individual fragments: a partially sent message must drain.
    pub async fn enqueue(
        &self,
        packet: Vec<u8>,
        cancel: Option<CancellationToken>,
        receipt: Option<oneshot::Sender<Result<()>>>,
    ) -> Result<()> {
        if packet.is_empty() || packet.len() > MAX_PDU {
            return Err(RdpError::Limit("outgoing transport packet"));
        }
        let channel = outgoing_channel(&packet);
        timeout(self.duration, async {
            loop {
                // Register before checking capacity so a concurrent completion
                // cannot be missed between releasing the lock and waiting.
                let available = self.shared.available.notified();
                tokio::pin!(available);
                available.as_mut().enable();
                {
                    let mut queue = self
                        .shared
                        .queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if let Some(error) = &queue.failure {
                        return Err(error.clone());
                    }
                    if packet.len() > queue.byte_limit || queue.packet_limit == 0 {
                        return Err(RdpError::Limit("transport writer capacity"));
                    }
                    if let Some(bytes) = queue
                        .bytes
                        .checked_add(packet.len())
                        .filter(|bytes| *bytes <= queue.byte_limit)
                        && queue.packets < queue.packet_limit
                    {
                        if !queue.channels.contains_key(&channel) {
                            queue.ready.push_back(channel);
                        }
                        queue
                            .channels
                            .entry(channel)
                            .or_default()
                            .push_back(Queued {
                                packet,
                                cancel,
                                receipt,
                            });
                        queue.bytes = bytes;
                        queue.packets += 1;
                        drop(queue);
                        self.shared.ready.notify_one();
                        return Ok(());
                    }
                }
                available.await;
            }
        })
        .await
        .map_err(|_| RdpError::Timeout)?
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
        let pending = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .fail(RdpError::Disconnected);
        self.shared.available.notify_waiters();
        reject_receipts(pending, RdpError::Disconnected);
    }
}

impl Queue {
    fn pop(&mut self) -> Option<Queued> {
        let channel = self.ready.pop_front()?;
        let entries = self
            .channels
            .get_mut(&channel)
            .expect("ready channel is queued");
        let packet = entries.pop_front().expect("ready channel has packets");
        if entries.is_empty() {
            self.channels.remove(&channel);
        } else {
            self.ready.push_back(channel);
        }
        Some(packet)
    }

    fn finished(&mut self, bytes: usize) {
        // Drop can abort the task while its write completes on another thread.
        if self.failure.is_none() {
            self.bytes -= bytes;
            self.packets -= 1;
        }
    }

    fn fail(&mut self, error: RdpError) -> BTreeMap<u16, VecDeque<Queued>> {
        self.failure = Some(error);
        self.bytes = 0;
        self.packets = 0;
        self.ready.clear();
        std::mem::take(&mut self.channels)
    }
}

fn reject_receipts(pending: BTreeMap<u16, VecDeque<Queued>>, error: RdpError) {
    for queued in pending.into_values().flatten() {
        if let Some(receipt) = queued.receipt {
            let _ = receipt.send(Err(error.clone()));
        }
    }
}

fn outgoing_channel(packet: &[u8]) -> u16 {
    // These are trusted packets encoded by rustrdp, never inbound network data.
    // TPKT (4) + X.224 Data (3) + MCS SendDataRequest tag (1) + initiator (2)
    // precede the big-endian channel ID at bytes 10..12. Fast-path input and
    // non-MCS control packets share group zero, retaining their own FIFO order.
    if packet.len() >= 12 && packet[..2] == [3, 0] && packet[4..8] == [2, 0xf0, 0x80, 0x64] {
        u16::from_be_bytes([packet[10], packet[11]])
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_packet(channel: u16, sequence: u8) -> Vec<u8> {
        let [high, low] = channel.to_be_bytes();
        vec![
            3, 0, 0, 15, 2, 0xf0, 0x80, 0x64, 0, 0, high, low, 0x70, 1, sequence,
        ]
    }

    #[tokio::test]
    async fn writer_round_robins_channels_without_reordering_their_fragments() {
        let (client, mut server) = tokio::io::duplex(32);
        let writer = Writer::new(Framed::new(client), Duration::from_secs(1));
        for packet in [
            channel_packet(1004, 1),
            channel_packet(1004, 2),
            channel_packet(1004, 3),
            channel_packet(1005, 1),
            channel_packet(1005, 2),
        ] {
            writer.enqueue(packet, None, None).await.unwrap();
        }
        let mut received = Vec::new();
        timeout(Duration::from_secs(1), async {
            for _ in 0..5 {
                let mut packet = [0; 15];
                server.read_exact(&mut packet).await.unwrap();
                received.push((outgoing_channel(&packet), packet[14]));
            }
        })
        .await
        .unwrap();
        assert_eq!(
            received,
            [(1004, 1), (1005, 1), (1004, 2), (1005, 2), (1004, 3)]
        );
    }

    #[tokio::test]
    async fn queued_paste_is_cancellable_and_receipts_follow_transport_flush() {
        let (client, mut server) = tokio::io::duplex(8);
        let writer = Writer::new(
            Framed::new(tokio::io::BufWriter::with_capacity(128, client)),
            Duration::from_secs(1),
        );
        let cancel = CancellationToken::new();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        writer
            .enqueue(
                channel_packet(1003, 1),
                Some(cancel.clone()),
                Some(cancel_tx),
            )
            .await
            .unwrap();
        cancel.cancel();
        let (sent_tx, mut sent_rx) = oneshot::channel();
        writer
            .enqueue(channel_packet(1003, 2), None, Some(sent_tx))
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), cancel_rx)
                .await
                .unwrap()
                .unwrap(),
            Err(RdpError::ClipboardUnavailable)
        );
        assert!(matches!(
            sent_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let mut packet = [0; 15];
        timeout(Duration::from_secs(1), server.read_exact(&mut packet))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet, channel_packet(1003, 2).as_slice());
        assert_eq!(
            timeout(Duration::from_secs(1), sent_rx)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
    }

    #[tokio::test]
    async fn writer_waits_for_in_flight_byte_and_packet_capacity() {
        for (byte_limit, packet_limit) in [(15, 8), (1024, 1)] {
            let (client, mut server) = tokio::io::duplex(1);
            let writer = Writer::with_limits(
                Framed::new(client),
                Duration::from_secs(1),
                byte_limit,
                packet_limit,
            );
            writer
                .enqueue(channel_packet(1004, 1), None, None)
                .await
                .unwrap();
            tokio::task::yield_now().await;
            let pending = writer.enqueue(channel_packet(1004, 2), None, None);
            tokio::pin!(pending);
            tokio::select! {
                biased;
                result = &mut pending => panic!("in-flight packet released capacity early: {result:?}"),
                _ = tokio::task::yield_now() => {},
            }
            timeout(Duration::from_secs(1), async {
                let mut received = [0; 15];
                server.read_exact(&mut received).await.unwrap();
                assert_eq!(received, channel_packet(1004, 1).as_slice());
                pending.await.unwrap();
                server.read_exact(&mut received).await.unwrap();
                assert_eq!(received, channel_packet(1004, 2).as_slice());
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn stalled_writer_times_out_instead_of_reporting_queue_overflow() {
        let (client, _server) = tokio::io::duplex(1);
        let writer = Writer::with_limits(Framed::new(client), Duration::from_millis(20), 15, 1);
        writer
            .enqueue(channel_packet(1004, 1), None, None)
            .await
            .unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(1),
                writer.enqueue(channel_packet(1004, 2), None, None),
            )
            .await
            .unwrap(),
            Err(RdpError::Timeout)
        );
    }

    #[tokio::test]
    async fn packets_that_cannot_fit_are_rejected_without_waiting() {
        let (client, _server) = tokio::io::duplex(1);
        let writer = Writer::with_limits(Framed::new(client), Duration::from_secs(1), 14, 1);
        tokio::select! {
            biased;
            result = writer.enqueue(channel_packet(1004, 1), None, None) => {
                assert_eq!(result, Err(RdpError::Limit("transport writer capacity")));
            }
            _ = tokio::task::yield_now() => panic!("an oversized packet waited for capacity"),
        }
    }

    #[tokio::test]
    async fn writer_failure_wakes_capacity_waiters() {
        let (client, server) = tokio::io::duplex(1);
        let writer = Writer::with_limits(Framed::new(client), Duration::from_secs(1), 15, 1);
        let (receipt, received) = oneshot::channel();
        writer
            .enqueue(channel_packet(1004, 1), None, Some(receipt))
            .await
            .unwrap();
        let pending = writer.enqueue(channel_packet(1004, 2), None, None);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("full queue accepted a packet: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        drop(server);
        assert_eq!(
            timeout(Duration::from_secs(1), pending).await.unwrap(),
            Err(RdpError::Connection)
        );
        assert_eq!(received.await.unwrap(), Err(RdpError::Connection));
    }

    #[tokio::test]
    async fn writer_failure_rejects_receipts_and_future_enqueue_attempts() {
        let (client, server) = tokio::io::duplex(32);
        drop(server);
        let mut writer = Writer::new(Framed::new(client), Duration::from_secs(1));
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        writer
            .enqueue(channel_packet(1004, 1), None, Some(first_tx))
            .await
            .unwrap();
        writer
            .enqueue(channel_packet(1005, 1), None, Some(second_tx))
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), writer.errors.recv())
                .await
                .unwrap(),
            Some(RdpError::Connection)
        );
        assert_eq!(first_rx.await.unwrap(), Err(RdpError::Connection));
        assert_eq!(second_rx.await.unwrap(), Err(RdpError::Connection));
        assert_eq!(
            writer.enqueue(channel_packet(1003, 1), None, None).await,
            Err(RdpError::Connection)
        );
    }

    #[tokio::test]
    async fn splitting_transport_preserves_already_buffered_incoming_frames() {
        let (client, mut server) = tokio::io::duplex(64);
        server
            .write_all(&[3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80, 0, 5, 6, 0, 0])
            .await
            .unwrap();
        let mut framed = Framed::new(client);
        assert_eq!(
            framed.read_pdu().await.unwrap().as_ref(),
            &[3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80]
        );
        assert_eq!(framed.buffered.as_ref(), &[0, 5, 6, 0, 0]);
        let (mut read, mut write) = framed.split();
        assert_eq!(read.read_pdu().await.unwrap().as_ref(), &[0, 5, 6, 0, 0]);
        write.write(&[0, 5, 6, 0, 0]).await.unwrap();
        let mut sent = [0; 5];
        server.read_exact(&mut sent).await.unwrap();
        assert_eq!(sent, [0, 5, 6, 0, 0]);
    }

    #[tokio::test]
    async fn writes_flush_buffered_transports_before_waiting_for_a_reply() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut framed = Framed::new(tokio::io::BufWriter::with_capacity(64, client));
        let packet = [0, 5, 6, 0, 0];
        framed.write(&packet).await.unwrap();
        let mut received = [0; 5];
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            server.read_exact(&mut received),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received, packet);
    }

    #[tokio::test]
    async fn frames_survive_partial_read_cancellation_and_coalesced_packets() {
        let (client, mut server) = tokio::io::duplex(128);
        let mut framed = Framed::new(client);
        server.write_all(&[3, 0, 0]).await.unwrap();
        tokio::select! {
            biased;
            result = framed.read_pdu() => panic!("incomplete frame returned {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert_eq!(framed.buffered.as_ref(), &[3, 0, 0]);
        server
            .write_all(&[9, 2, 0xf0, 0x80, 0x21, 0x80, 0, 5, 6, 0, 0])
            .await
            .unwrap();
        assert_eq!(
            framed.read_pdu().await.unwrap().as_ref(),
            &[3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80]
        );
        assert_eq!(framed.read_pdu().await.unwrap().as_ref(), &[0, 5, 6, 0, 0]);
    }

    #[tokio::test]
    async fn credssp_and_early_auth_do_not_consume_the_next_stage() {
        let (client, mut server) = tokio::io::duplex(128);
        let mut framed = Framed::new(client);
        server
            .write_all(&[0x30, 2, 5, 0, 0, 0, 0, 0, 0, 5, 6, 0, 0])
            .await
            .unwrap();
        assert_eq!(framed.read_credssp().await.unwrap(), [0x30, 2, 5, 0]);
        assert_eq!(framed.read_early_auth().await.unwrap(), [0; 4]);
        assert_eq!(framed.read_pdu().await.unwrap().as_ref(), &[0, 5, 6, 0, 0]);
    }

    #[tokio::test]
    async fn coalesced_frames_share_storage_without_copying_the_remainder() {
        let packets = [0, 5, 6, 0, 0, 0, 5, 7, 0, 0];
        let mut framed = Framed::new(packets.as_slice());
        let first = framed.read_pdu().await.unwrap();
        let remainder = framed.buffered.as_ptr();
        assert_eq!(first.as_ptr().wrapping_add(first.len()), remainder);
        let second = framed.read_pdu().await.unwrap();
        assert_eq!(second.as_ptr(), remainder);
        assert_eq!(first.as_ref(), &packets[..5]);
        assert_eq!(second.as_ref(), &packets[5..]);
        assert!(framed.into_stream().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retained_frames_survive_receive_buffer_growth() {
        let length = READ_CHUNK + 16;
        let mut packets = vec![0, 5, 6, 0, 0];
        packets.extend_from_slice(&[3, 0]);
        packets.extend_from_slice(&(length as u16).to_be_bytes());
        packets.resize(5 + length, 42);
        packets.extend_from_slice(&[0, 5, 7, 0, 0]);
        let mut framed = Framed::new(packets.as_slice());
        let first = framed.read_pdu().await.unwrap();
        let large = framed.read_pdu().await.unwrap();
        let last = framed.read_pdu().await.unwrap();
        assert_eq!(first.as_ref(), &packets[..5]);
        assert_eq!(large.as_ref(), &packets[5..5 + length]);
        assert_eq!(last.as_ref(), &packets[5 + length..]);
    }

    #[tokio::test]
    async fn oversized_authentication_frame_is_rejected_before_reading_its_body() {
        let oversized = [0x30, 0x83, 0x01, 0x00, 0x01];
        let mut framed = Framed::new(oversized.as_slice());
        assert_eq!(
            framed.read_credssp().await,
            Err(RdpError::Limit("CredSSP message"))
        );
        assert_eq!(framed.buffered.len(), oversized.len());
    }

    #[tokio::test]
    async fn tls_upgrade_rejects_unconsumed_plaintext() {
        let packets = [0, 5, 6, 0, 0, 0, 5, 7, 0, 0];
        let mut framed = Framed::new(packets.as_slice());
        framed.read_pdu().await.unwrap();
        assert_eq!(
            framed.into_stream(),
            Err(RdpError::Protocol("unconsumed transport input"))
        );
    }
}
