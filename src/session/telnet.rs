use crate::i18n::messages as i18n_key;

use super::{
    ActorCommand, FailureKind, SessionState, actor,
    state::Shared,
    transport::{
        OUTPUT_CAPACITY, Output, Transport, WRITE_CAPACITY, WRITE_STALL_TIMEOUT, WriteCommand,
        write_all_cancellable,
    },
};
use crate::terminal::Terminal;
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, oneshot};

#[derive(Clone, Debug)]
pub struct TelnetOptions {
    pub host: String,
    pub port: u16,
    pub timeout_secs: u64,
}

type TelnetResult<T> = std::result::Result<T, (FailureKind, String)>;

pub(super) async fn run(
    options: TelnetOptions,
    terminal: Terminal,
    columns: u16,
    rows: u16,
    commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
) {
    let transport = match connect(options, columns, rows, shared.clone()).await {
        Ok(transport) => Some(transport),
        Err((kind, message)) => {
            if !matches!(shared.current_state(), SessionState::Failed { .. }) {
                if shared.io_stop.is_cancelled() {
                    shared.state(SessionState::Disconnected { exit_code: None });
                } else {
                    shared.failed(kind, message);
                }
            }
            None
        }
    };
    actor::run(terminal, commands, shared, transport).await;
}

async fn connect(
    options: TelnetOptions,
    columns: u16,
    rows: u16,
    shared: Arc<Shared>,
) -> TelnetResult<Transport> {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let connect = async {
        let addresses: Vec<_> = tokio::net::lookup_host((options.host.as_str(), options.port))
            .await
            .map_err(|error| (FailureKind::Dns, error.to_string()))?
            .collect();
        let stream = TcpStream::connect(addresses.as_slice())
            .await
            .map_err(|error| (FailureKind::Network, error.to_string()))?;
        stream
            .set_nodelay(true)
            .map_err(|error| (FailureKind::Network, error.to_string()))?;
        Ok::<_, (FailureKind, String)>(stream)
    };
    let stream = tokio::select! {
        _ = shared.io_stop.cancelled() => return Err((FailureKind::Network, String::new())),
        result = tokio::time::timeout_at(deadline, connect) => result
            .map_err(|_| (FailureKind::Timeout, crate::i18n::tr(i18n_key::CONNECTIONS_TELNET_CONNECTION_TIMED_OUT).into()))??,
    };
    Ok(transport(stream, columns, rows, shared))
}

fn transport(stream: TcpStream, columns: u16, rows: u16, shared: Arc<Shared>) -> Transport {
    let (mut reader, mut writer) = stream.into_split();
    let codec = Arc::new(Mutex::new(Codec::new(columns, rows)));
    let controls = Arc::new(Notify::new());
    let (output_tx, output) = mpsc::channel(OUTPUT_CAPACITY);
    let (input, mut write_rx) = mpsc::channel(WRITE_CAPACITY);
    let (exit_tx, exit) = oneshot::channel();
    let read_shared = shared.clone();
    let read_codec = codec.clone();
    let read_controls = controls.clone();
    let read_output = output_tx.clone();
    let reader_task = tokio::spawn(async move {
        let mut buffer = [0; 8192];
        loop {
            let result = tokio::select! {
                _ = read_shared.io_stop.cancelled() => break,
                result = reader.read(&mut buffer) => result,
            };
            let bytes = match result {
                Ok(0) => {
                    let complete = read_codec
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .complete();
                    if !complete {
                        let _ = read_output
                            .send(Output::Error(
                                FailureKind::Protocol,
                                crate::i18n::tr(i18n_key::CONNECTIONS_THE_TELNET_SERVER_CLOSED_AN_INCOMPLETE_COMMAND)
                                    .into(),
                            ))
                            .await;
                    }
                    break;
                }
                Ok(length) => {
                    let (result, controls_pending) = {
                        let mut codec =
                            read_codec.lock().unwrap_or_else(|error| error.into_inner());
                        let result = codec.receive(&buffer[..length]);
                        read_shared
                            .local_echo
                            .store(!codec.remote[ECHO as usize].enabled(), Ordering::Release);
                        (result, !codec.controls.is_empty())
                    };
                    if controls_pending {
                        read_controls.notify_one();
                    }
                    match result {
                        Ok(bytes) => bytes,
                        Err(message) => {
                            let _ = read_output
                                .send(Output::Error(FailureKind::Protocol, message.into()))
                                .await;
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = read_output
                        .send(Output::Error(FailureKind::Read, error.to_string()))
                        .await;
                    break;
                }
            };
            if !bytes.is_empty() && read_output.send(Output::Data(bytes)).await.is_err() {
                break;
            }
        }
        let _ = exit_tx.send(Ok(None));
        let _ = read_output.send(Output::Eof).await;
    });
    let write_shared = shared.clone();
    let writer_task = tokio::spawn(async move {
        // Keep negotiation replies and user data ordered with mode changes. A
        // reader cannot enable BINARY between taking replies and encoding data.
        let mut initial = true;
        loop {
            let command = if initial {
                initial = false;
                None
            } else {
                tokio::select! {
                    _ = write_shared.io_stop.cancelled() => break,
                    _ = controls.notified() => None,
                    command = write_rx.recv() => match command {
                        Some(command) => Some(command),
                        None => break,
                    },
                }
            };
            let tracked = matches!(&command, Some(WriteCommand::UserInput(_)));
            let bytes = {
                let mut codec = codec.lock().unwrap_or_else(|error| error.into_inner());
                let mut bytes = std::mem::take(&mut codec.controls);
                match command {
                    Some(WriteCommand::Data(data)) => codec.encode(&data, false, &mut bytes),
                    Some(WriteCommand::UserInput(data)) => codec.encode(&data, true, &mut bytes),
                    Some(WriteCommand::Resize(columns, rows)) => {
                        codec.resize(columns, rows, &mut bytes)
                    }
                    None => {}
                }
                bytes
            };
            let result = write_all_cancellable(
                &mut writer,
                &bytes,
                &write_shared.io_stop,
                WRITE_STALL_TIMEOUT,
            )
            .await;
            if write_shared.io_stop.is_cancelled() {
                break;
            }
            let (kind, message) = match result {
                Ok(()) => {
                    if tracked {
                        write_shared.input_completed();
                    }
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => (
                    FailureKind::Timeout,
                    crate::i18n::tr(i18n_key::CONNECTIONS_TELNET_WRITE_TIMED_OUT).into(),
                ),
                Err(error) => (FailureKind::Write, error.to_string()),
            };
            let _ = output_tx.send(Output::Error(kind, message)).await;
            break;
        }
    });
    shared.live_state(SessionState::Connected);
    Transport {
        output,
        input,
        exit,
        tasks: vec![reader_task, writer_task],
        reaper: None,
    }
}

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const BINARY: u8 = 0;
const ECHO: u8 = 1;
const SGA: u8 = 3;
const TERMINAL_TYPE: u8 = 24;
const NAWS: u8 = 31;
const MAX_SUBNEGOTIATION: usize = 4096;
const MAX_CONTROLS: usize = 64 * 1024;

/// RFC 1143: the queue bit remembers an opposite request during negotiation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OptionState {
    #[default]
    No,
    Yes,
    WantNo(bool),
    WantYes(bool),
}

impl OptionState {
    fn enabled(self) -> bool {
        self == Self::Yes
    }

    fn request(&mut self, enabled: bool) -> Option<bool> {
        use OptionState::*;
        let (next, reply) = match (*self, enabled) {
            (No, true) => (WantYes(false), Some(true)),
            (Yes, false) => (WantNo(false), Some(false)),
            (WantNo(_), true) => (WantNo(true), None),
            (WantNo(_), false) => (WantNo(false), None),
            (WantYes(_), false) => (WantYes(true), None),
            (WantYes(_), true) => (WantYes(false), None),
            (state, _) => (state, None),
        };
        *self = next;
        reply
    }

    fn receive(&mut self, positive: bool, supported: bool) -> Option<bool> {
        use OptionState::*;
        let (next, reply) = match (*self, positive) {
            (No, true) if supported => (Yes, Some(true)),
            (No, true) => (No, Some(false)),
            (Yes, true) | (No, false) => (*self, None),
            (WantNo(false), _) | (WantYes(_), false) => (No, None),
            (WantNo(true), true) | (WantYes(false), true) => (Yes, None),
            (WantYes(true), true) => (WantNo(false), Some(false)),
            (Yes, false) => (No, Some(false)),
            (WantNo(true), false) => (WantYes(false), Some(true)),
        };
        *self = next;
        reply
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParseState {
    #[default]
    Data,
    Command,
    Option(u8),
    SubOption,
    SubData(u8),
    SubCommand(u8),
}

struct Codec {
    local: [OptionState; 256],
    remote: [OptionState; 256],
    state: ParseState,
    subnegotiation: Vec<u8>,
    controls: Vec<u8>,
    after_cr: bool,
    columns: u16,
    rows: u16,
}

impl Codec {
    fn new(columns: u16, rows: u16) -> Self {
        let mut codec = Self {
            local: [OptionState::No; 256],
            remote: [OptionState::No; 256],
            state: ParseState::Data,
            subnegotiation: Vec::new(),
            controls: Vec::new(),
            after_cr: false,
            columns,
            rows,
        };
        for option in [BINARY, SGA, TERMINAL_TYPE, NAWS] {
            codec.local[option as usize].request(true);
            codec.controls.extend_from_slice(&[IAC, WILL, option]);
        }
        for option in [BINARY, ECHO, SGA] {
            codec.remote[option as usize].request(true);
            codec.controls.extend_from_slice(&[IAC, DO, option]);
        }
        codec
    }

    fn receive(&mut self, bytes: &[u8]) -> std::result::Result<Vec<u8>, &'static str> {
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match self.state {
                ParseState::Data => {
                    if byte == IAC {
                        self.state = ParseState::Command;
                    } else {
                        self.data(byte, &mut output);
                    }
                }
                ParseState::Command => {
                    self.state = ParseState::Data;
                    match byte {
                        IAC => self.data(IAC, &mut output),
                        WILL | WONT | DO | DONT => self.state = ParseState::Option(byte),
                        SB => self.state = ParseState::SubOption,
                        // AYT, EC and EL are optional NVT commands; no data is
                        // reflected to the peer except this fixed AYT response.
                        246 => self.controls.extend_from_slice(b"\r\n[Yes]\r\n"),
                        247 => output.extend_from_slice(b"\x08 \x08"),
                        248 => output.extend_from_slice(b"\r\x1b[K"),
                        _ => {}
                    }
                }
                ParseState::Option(command) => {
                    self.negotiate(command, byte);
                    self.state = ParseState::Data;
                }
                ParseState::SubOption => {
                    self.subnegotiation.clear();
                    self.state = ParseState::SubData(byte);
                }
                ParseState::SubData(option) => {
                    if byte == IAC {
                        self.state = ParseState::SubCommand(option);
                    } else {
                        self.subnegotiation.push(byte);
                    }
                }
                ParseState::SubCommand(option) => match byte {
                    IAC => {
                        self.subnegotiation.push(IAC);
                        self.state = ParseState::SubData(option);
                    }
                    SE => {
                        self.subnegotiate(option);
                        self.state = ParseState::Data;
                    }
                    _ => {
                        return Err(crate::i18n::tr(
                            i18n_key::CONNECTIONS_INVALID_TELNET_SUBNEGOTIATION,
                        ));
                    }
                },
            }
            if self.subnegotiation.len() > MAX_SUBNEGOTIATION {
                return Err(crate::i18n::tr(
                    i18n_key::CONNECTIONS_TELNET_SUBNEGOTIATION_EXCEEDS_THE_SIZE_LIMIT,
                ));
            }
            if self.controls.len() > MAX_CONTROLS {
                return Err(crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_TELNET_SERVER_SENT_TOO_MANY_NEGOTIATION_REQUESTS,
                ));
            }
        }
        Ok(output)
    }

    fn data(&mut self, byte: u8, output: &mut Vec<u8>) {
        let binary = self.remote[BINARY as usize].enabled();
        let skip = !binary && self.after_cr && byte == 0;
        self.after_cr = !binary && byte == b'\r';
        if !skip {
            output.push(byte);
        }
    }

    fn negotiate(&mut self, command: u8, option: u8) {
        let local = matches!(command, DO | DONT);
        let positive = matches!(command, DO | WILL);
        let supported = if local {
            matches!(option, BINARY | SGA | TERMINAL_TYPE | NAWS)
        } else {
            matches!(option, BINARY | ECHO | SGA)
        };
        let state = if local {
            &mut self.local[option as usize]
        } else {
            &mut self.remote[option as usize]
        };
        let enabled = state.enabled();
        let reply = state.receive(positive, supported);
        let now_enabled = state.enabled();
        if let Some(positive) = reply {
            let command = match (local, positive) {
                (true, true) => WILL,
                (true, false) => WONT,
                (false, true) => DO,
                (false, false) => DONT,
            };
            self.controls.extend_from_slice(&[IAC, command, option]);
        }
        if local && option == NAWS && !enabled && now_enabled {
            Self::window_size(self.columns, self.rows, &mut self.controls);
        }
        if !local && option == BINARY && enabled != now_enabled {
            self.after_cr = false;
        }
    }

    fn subnegotiate(&mut self, option: u8) {
        if option == TERMINAL_TYPE
            && self.local[TERMINAL_TYPE as usize].enabled()
            && self.subnegotiation == [1]
        {
            self.controls
                .extend_from_slice(&[IAC, SB, TERMINAL_TYPE, 0]);
            self.controls.extend_from_slice(b"xterm-256color");
            self.controls.extend_from_slice(&[IAC, SE]);
        }
        self.subnegotiation.clear();
    }

    fn encode(&self, data: &[u8], user_input: bool, output: &mut Vec<u8>) {
        let binary = self.local[BINARY as usize].enabled();
        let mut bytes = data.iter().copied().peekable();
        while let Some(byte) = bytes.next() {
            if !binary && byte == b'\r' {
                output.push(b'\r');
                if bytes.peek() == Some(&b'\n') {
                    bytes.next();
                    output.push(b'\n');
                } else if bytes.peek() == Some(&0) {
                    bytes.next();
                    output.push(0);
                } else {
                    output.push(if user_input { b'\n' } else { 0 });
                }
            } else {
                Self::escaped(byte, output);
            }
        }
    }

    fn escaped(byte: u8, output: &mut Vec<u8>) {
        output.push(byte);
        if byte == IAC {
            output.push(IAC);
        }
    }

    fn window_size(columns: u16, rows: u16, output: &mut Vec<u8>) {
        output.extend_from_slice(&[IAC, SB, NAWS]);
        for byte in columns.to_be_bytes().into_iter().chain(rows.to_be_bytes()) {
            Self::escaped(byte, output);
        }
        output.extend_from_slice(&[IAC, SE]);
    }

    fn resize(&mut self, columns: u16, rows: u16, output: &mut Vec<u8>) {
        if (self.columns, self.rows) == (columns, rows) {
            return;
        }
        self.columns = columns;
        self.rows = rows;
        if self.local[NAWS as usize].enabled() {
            Self::window_size(columns, rows, output);
        }
    }

    fn complete(&self) -> bool {
        self.state == ParseState::Data
    }
}

pub(super) fn local_echo(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut bytes = bytes.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\r' => {
                if bytes.peek() == Some(&b'\n') {
                    bytes.next();
                }
                output.extend_from_slice(b"\r\n");
            }
            8 | 127 => output.extend_from_slice(b"\x08 \x08"),
            _ => output.push(byte),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn fresh_codec() -> Codec {
        let mut codec = Codec::new(80, 24);
        codec.controls.clear();
        codec
    }

    #[test]
    fn fragmented_commands_and_nvt_data_preserve_terminal_bytes() {
        let mut codec = fresh_codec();
        let mut output = Vec::new();
        for byte in b"hello\r\0world\r\n\xff\xff\xff\xfb\x01!" {
            output.extend(codec.receive(&[*byte]).unwrap());
        }
        assert_eq!(output, b"hello\rworld\r\n\xff!");
        assert!(codec.remote[ECHO as usize].enabled());
        assert!(codec.controls.is_empty());
        assert!(codec.complete());
    }

    #[test]
    fn unknown_options_are_refused_without_refusal_loops() {
        let mut codec = fresh_codec();
        codec.receive(&[IAC, WILL, 99, IAC, DO, 99]).unwrap();
        assert_eq!(codec.controls, [IAC, DONT, 99, IAC, WONT, 99]);
        codec.controls.clear();
        codec.receive(&[IAC, WONT, 99, IAC, DONT, 99]).unwrap();
        assert!(codec.controls.is_empty());
    }

    #[test]
    fn binary_mode_is_negotiated_independently_and_still_escapes_iac() {
        let mut codec = fresh_codec();
        let mut output = Vec::new();
        codec.encode(b"a\rb\r\n\xff", false, &mut output);
        assert_eq!(output, b"a\r\0b\r\n\xff\xff");
        output.clear();
        codec.encode(b"\r", true, &mut output);
        assert_eq!(output, b"\r\n");
        codec.receive(&[IAC, DO, BINARY]).unwrap();
        output.clear();
        codec.encode(b"a\r\0\xff", false, &mut output);
        assert_eq!(output, b"a\r\0\xff\xff");
        assert_eq!(codec.receive(b"\r\0").unwrap(), b"\r");
        codec.receive(&[IAC, WILL, BINARY]).unwrap();
        assert_eq!(codec.receive(b"\r\0\xff\xff").unwrap(), b"\r\0\xff");
        codec.receive(&[IAC, WONT, BINARY]).unwrap();
        assert_eq!(codec.receive(b"\r\0").unwrap(), b"\r");
    }

    #[test]
    fn terminal_type_requires_agreement_and_handles_fragmentation() {
        let mut codec = fresh_codec();
        let request = [IAC, SB, TERMINAL_TYPE, 1, IAC, SE];
        codec.receive(&request).unwrap();
        assert!(codec.controls.is_empty());
        codec.receive(&[IAC, DO, TERMINAL_TYPE]).unwrap();
        for byte in request {
            codec.receive(&[byte]).unwrap();
        }
        assert_eq!(codec.controls, b"\xff\xfa\x18\0xterm-256color\xff\xf0");
    }

    #[test]
    fn naws_sends_latest_size_on_agreement_and_stops_on_dont() {
        let mut codec = fresh_codec();
        let mut output = Vec::new();
        codec.resize(255, 40, &mut output);
        assert!(output.is_empty());
        codec.receive(&[IAC, DO, NAWS]).unwrap();
        assert_eq!(codec.controls, [IAC, SB, NAWS, 0, 255, 255, 0, 40, IAC, SE]);
        codec.resize(100, 30, &mut output);
        assert_eq!(output, [IAC, SB, NAWS, 0, 100, 0, 30, IAC, SE]);
        codec.receive(&[IAC, DONT, NAWS]).unwrap();
        output.clear();
        codec.resize(110, 35, &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn q_method_queues_opposite_requests_and_obeys_rejection() {
        let mut state = OptionState::No;
        assert_eq!(state.request(true), Some(true));
        assert_eq!(state.request(false), None);
        assert_eq!(state.receive(true, true), Some(false));
        assert_eq!(state, OptionState::WantNo(false));
        assert_eq!(state.request(true), None);
        assert_eq!(state.receive(false, true), Some(true));
        assert_eq!(state, OptionState::WantYes(false));
        assert_eq!(state.receive(false, true), None);
        assert_eq!(state, OptionState::No);
    }

    #[test]
    fn subnegotiations_are_bounded_and_malformed_commands_are_rejected() {
        let mut codec = fresh_codec();
        codec.receive(&[IAC, SB, 99]).unwrap();
        assert!(codec.receive(&vec![0; MAX_SUBNEGOTIATION + 1]).is_err());
        let mut codec = fresh_codec();
        assert!(codec.receive(&[IAC, SB, 99, IAC, WILL]).is_err());
        let mut codec = fresh_codec();
        codec.receive(&[IAC, SB, 99, IAC, IAC, IAC, SE]).unwrap();
        assert!(codec.complete());
        codec.receive(&[IAC]).unwrap();
        assert!(!codec.complete());
    }

    #[test]
    fn negotiation_reply_backlog_is_bounded() {
        let mut codec = fresh_codec();
        let requests = [IAC, WILL, 99].repeat(MAX_CONTROLS / 3 + 1);
        assert!(codec.receive(&requests).is_err());
        assert!(codec.controls.len() <= MAX_CONTROLS + 3);
    }

    #[test]
    fn echo_changes_do_not_loop_and_local_echo_handles_editing_keys() {
        let mut codec = fresh_codec();
        codec.receive(&[IAC, WILL, ECHO, IAC, WILL, ECHO]).unwrap();
        assert!(codec.remote[ECHO as usize].enabled());
        assert!(codec.controls.is_empty());
        codec.receive(&[IAC, WONT, ECHO, IAC, WONT, ECHO]).unwrap();
        assert!(!codec.remote[ECHO as usize].enabled());
        assert_eq!(codec.controls, [IAC, DONT, ECHO]);
        assert_eq!(local_echo(b"a\x7fb\r\nc\r"), b"a\x08 \x08b\r\nc\r\n");
    }

    #[tokio::test]
    async fn loopback_transports_input_resize_and_retains_output_before_eof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut initial = [0; 21];
            stream.read_exact(&mut initial).await.unwrap();
            stream
                .write_all(&[IAC, DO, BINARY, IAC, WILL, ECHO, IAC, DO, NAWS])
                .await
                .unwrap();
            stream.write_all(b"ready\r\n").await.unwrap();
            let mut size = [0; 9];
            stream.read_exact(&mut size).await.unwrap();
            assert_eq!(size, [IAC, SB, NAWS, 0, 80, 0, 24, IAC, SE]);
            let mut input = [0; 4];
            stream.read_exact(&mut input).await.unwrap();
            assert_eq!(input, [b'a', IAC, IAC, b'\r']);
            stream.read_exact(&mut size).await.unwrap();
            assert_eq!(size, [IAC, SB, NAWS, 0, 100, 0, 30, IAC, SE]);
            stream.write_all(b"tail").await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            let terminal = Terminal::new(80, 24, 0);
            let shared = Shared::new(&terminal);
            shared.local_echo.store(true, Ordering::Release);
            let mut transport = connect(
                TelnetOptions {
                    host: address.ip().to_string(),
                    port: address.port(),
                    timeout_secs: 1,
                },
                80,
                24,
                shared.clone(),
            )
            .await
            .unwrap();
            let mut output = Vec::new();
            while output.len() < b"ready\r\n".len() {
                match transport.output.recv().await.unwrap() {
                    Output::Data(bytes) => output.extend(bytes),
                    _ => panic!("unexpected transport event"),
                }
            }
            assert!(!shared.local_echo.load(Ordering::Acquire));
            transport
                .input
                .send(WriteCommand::UserInput(vec![b'a', IAC, b'\r']))
                .await
                .unwrap();
            transport
                .input
                .send(WriteCommand::Resize(100, 30))
                .await
                .unwrap();
            loop {
                match transport.output.recv().await.unwrap() {
                    Output::Data(bytes) => output.extend(bytes),
                    Output::Eof => break,
                    Output::Error(_, message) => panic!("{message}"),
                }
            }
            assert_eq!(output, b"ready\r\ntail");
            assert_eq!(transport.exit.await.unwrap().unwrap(), None);
            shared.io_stop.cancel();
            for task in transport.tasks {
                task.await.unwrap();
            }
            peer.await.unwrap();
        })
        .await
        .unwrap();
    }
}
