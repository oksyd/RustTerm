use super::super::transport::Output;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// One transport message at a time; parsing never monopolizes a runtime worker.
pub(super) struct OutputBuffer {
    bytes: Vec<u8>,
    offset: usize,
    quantum: usize,
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            offset: 0,
            quantum: 2048,
        }
    }
}

impl OutputBuffer {
    pub(super) fn pending(&self) -> bool {
        self.offset < self.bytes.len()
    }

    pub(super) async fn next(&mut self, receiver: &mut mpsc::Receiver<Output>) -> Option<Output> {
        if !self.pending() {
            match receiver.recv().await? {
                Output::Data(bytes) => {
                    self.bytes = bytes;
                    self.offset = 0;
                }
                event => return Some(event),
            }
        }
        let end = (self.offset + self.quantum).min(self.bytes.len());
        let bytes = if self.offset == 0 && end == self.bytes.len() {
            std::mem::take(&mut self.bytes)
        } else {
            let bytes = self.bytes[self.offset..end].to_vec();
            self.offset = end;
            bytes
        };
        Some(Output::Data(bytes))
    }

    pub(super) fn parsed(&mut self, bytes: usize, started: Instant) {
        let micros = started.elapsed().as_micros().max(1);
        // Adapt to the actual parser workload, bounded even for tiny or cheap packets.
        self.quantum = ((bytes as u128 * Duration::from_millis(1).as_micros()) / micros)
            .clamp(512, 8192) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chunks_preserve_every_byte_before_eof() {
        let (send, mut receive) = mpsc::channel(2);
        let expected = "\x1b[31m中文😀e\u{301}\x1b[0m\r\n"
            .repeat(1000)
            .into_bytes();
        send.send(Output::Data(expected.clone())).await.unwrap();
        send.send(Output::Eof).await.unwrap();
        let mut buffer = OutputBuffer::default();
        let mut actual = Vec::new();
        while let Some(Output::Data(bytes)) = buffer.next(&mut receive).await {
            assert!(bytes.len() <= 2048);
            actual.extend(bytes);
        }
        assert_eq!(actual, expected);
        assert!(!buffer.pending());
    }
}
