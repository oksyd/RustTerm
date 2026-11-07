use super::{Error, Result, http::Connection};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct LogStream {
    receive: mpsc::Receiver<Result<(bool, Vec<u8>)>>,
    stop: CancellationToken,
}
impl Drop for LogStream {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl LogStream {
    pub(super) fn start(
        mut body: Incoming,
        connection: Connection,
        tty: bool,
        parent: CancellationToken,
    ) -> Self {
        let stop = parent.child_token();
        let cancelled = stop.clone();
        let (send, receive) = mpsc::channel(16);
        tokio::spawn(async move {
            let _connection = connection;
            let pump = async {
                let mut decoder = Decoder::new(tty);
                while let Some(frame) = body.frame().await {
                    let frame = frame.map_err(|e| Error::Transport(e.to_string()))?;
                    if let Ok(data) = frame.into_data() {
                        for chunk in decoder.push(&data)? {
                            if send.send(Ok(chunk)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
                decoder.finish()
            };
            tokio::select! { _ = cancelled.cancelled() => {}, result=pump => { if let Err(error)=result { let _=send.send(Err(error)).await; } } }
        });
        Self { receive, stop }
    }
    pub async fn next(&mut self) -> Option<Result<(bool, Vec<u8>)>> {
        self.receive.recv().await
    }
}
/// Docker's 8-byte multiplex framing may be split at any HTTP or SSH boundary.
pub(super) struct Decoder {
    tty: bool,
    header: Vec<u8>,
    remaining: usize,
    stderr: bool,
}
impl Decoder {
    pub(super) fn new(tty: bool) -> Self {
        Self {
            tty,
            header: Vec::with_capacity(8),
            remaining: 0,
            stderr: false,
        }
    }
    pub(super) fn push(&mut self, mut data: &[u8]) -> Result<Vec<(bool, Vec<u8>)>> {
        if self.tty {
            return Ok(vec![(false, data.to_vec())]);
        }
        let mut output = Vec::new();
        while !data.is_empty() {
            if self.remaining == 0 {
                let n = (8 - self.header.len()).min(data.len());
                self.header.extend_from_slice(&data[..n]);
                data = &data[n..];
                if self.header.len() != 8 {
                    break;
                }
                if !matches!(self.header[0], 1 | 2) || self.header[1..4] != [0, 0, 0] {
                    return Err(Error::InvalidResponse);
                }
                self.stderr = self.header[0] == 2;
                self.remaining =
                    u32::from_be_bytes(self.header[4..8].try_into().expect("four byte length"))
                        as usize;
                self.header.clear();
                if self.remaining > 16 * 1024 * 1024 {
                    return Err(Error::Limit);
                }
                if self.remaining == 0 {
                    continue;
                }
            }
            let n = self.remaining.min(data.len());
            if n > 0 {
                output.push((self.stderr, data[..n].to_vec()));
                data = &data[n..];
                self.remaining -= n;
            }
        }
        Ok(output)
    }
    pub(super) fn finish(&self) -> Result<()> {
        if self.remaining == 0 && self.header.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidResponse)
        }
    }
}
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
#[derive(Clone)]
pub struct LogLine {
    pub text: String,
    pub stderr: bool,
}
#[derive(Default)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    pending: [Vec<u8>; 2],
    bytes: usize,
    pub evicted: usize,
}
impl LogBuffer {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }
    pub fn append(&mut self, stderr: bool, data: &[u8]) {
        let stream = usize::from(stderr);
        for &byte in data {
            if byte == b'\n' {
                self.flush(stream, false);
            } else {
                self.pending[stream].push(byte);
                if self.pending[stream].len() >= MAX_LINE_BYTES {
                    self.flush(stream, true);
                }
            }
        }
    }
    pub fn finish(&mut self) {
        for stream in 0..2 {
            if !self.pending[stream].is_empty() {
                self.flush(stream, false);
            }
        }
    }
    fn flush(&mut self, stream: usize, split: bool) {
        let bytes = std::mem::take(&mut self.pending[stream]);
        // Preserve partial UTF-8 at an artificial line-length boundary.
        let cut = if split {
            match std::str::from_utf8(&bytes) {
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                _ => bytes.len(),
            }
        } else {
            bytes.len()
        };
        let text = String::from_utf8_lossy(&bytes[..cut])
            .trim_end_matches('\r')
            .to_owned();
        self.pending[stream].extend_from_slice(&bytes[cut..]);
        self.bytes += text.len();
        self.lines.push_back(LogLine {
            text,
            stderr: stream == 1,
        });
        while self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES {
            if let Some(line) = self.lines.pop_front() {
                self.bytes -= line.text.len();
                self.evicted += 1;
            }
        }
    }
    pub fn text(&self, search: &str) -> String {
        let search = search.to_lowercase();
        self.lines
            .iter()
            .filter(|line| line.text.to_lowercase().contains(&search))
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
