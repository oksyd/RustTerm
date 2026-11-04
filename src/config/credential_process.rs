//! A stuck desktop Secret Service must not strand the application executor or shutdown.
//! The worker is the same executable, entered before graphics initialization. Authentication
//! data travels only over bounded anonymous pipes, never arguments, environment or files.

use super::credentials::{
    CredentialError, CredentialRef, CredentialStore, Secret, SystemCredentialStore,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

const MAX_SECRET_BYTES: usize = 65_536;
// JSON can expand each input byte to an ASCII \uXXXX escape.
const MAX_WIRE_BYTES: usize = MAX_SECRET_BYTES * 6 + 1024;

pub struct ProcessCredentialStore {
    timeout: Duration,
}

impl Default for ProcessCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessCredentialStore {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }

    fn request(
        &self,
        operation: Operation,
        reference: CredentialRef,
        secret: Option<&Secret>,
    ) -> Result<Reply, CredentialError> {
        if reference.0.is_nil() {
            return Err(CredentialError::InvalidReference);
        }
        if secret.is_some_and(|value| value.expose().len() > MAX_SECRET_BYTES) {
            return Err(CredentialError::Unavailable);
        }
        let request = Request {
            operation,
            reference,
            secret: secret.map(|value| value.expose().to_owned()),
        };
        let wire =
            Zeroizing::new(serde_json::to_vec(&request).map_err(|_| CredentialError::Unavailable)?);
        #[cfg(any(unix, windows))]
        {
            // Keep using this process's image after an update atomically replaces
            // its pathname. current_exe() can then end with " (deleted)" on Linux.
            #[cfg(target_os = "linux")]
            let executable = "/proc/self/exe";
            #[cfg(not(target_os = "linux"))]
            let executable = std::env::current_exe().map_err(|_| CredentialError::Unavailable)?;
            let mut command = std::process::Command::new(executable);
            command.arg("--credential-worker");
            exchange(&mut command, &wire, self.timeout, operation.mutates())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (&wire, self.timeout);
            Err(CredentialError::Unavailable)
        }
    }
}

impl CredentialStore for ProcessCredentialStore {
    fn get(&self, reference: CredentialRef) -> Result<Secret, CredentialError> {
        let mut reply = self.request(Operation::Get, reference, None)?;
        reply.result()?;
        reply
            .secret
            .take()
            .map(Secret::new)
            .ok_or(CredentialError::Unavailable)
    }
    fn set(&self, reference: CredentialRef, secret: &Secret) -> Result<(), CredentialError> {
        self.request(Operation::Set, reference, Some(secret))?
            .result()
    }
    fn delete(&self, reference: CredentialRef) -> Result<(), CredentialError> {
        self.request(Operation::Delete, reference, None)?.result()
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Get,
    Set,
    Delete,
}
impl Operation {
    fn mutates(self) -> bool {
        matches!(self, Self::Set | Self::Delete)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: Operation,
    reference: CredentialRef,
    secret: Option<String>,
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.secret {
            secret.zeroize();
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Missing,
    Unavailable,
    InvalidReference,
    OutcomeUnknown,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    status: Status,
    secret: Option<String>,
}
impl Reply {
    fn result(&self) -> Result<(), CredentialError> {
        match self.status {
            Status::Ok => Ok(()),
            Status::Missing => Err(CredentialError::Missing),
            Status::Unavailable => Err(CredentialError::Unavailable),
            Status::InvalidReference => Err(CredentialError::InvalidReference),
            Status::OutcomeUnknown => Err(CredentialError::OutcomeUnknown),
        }
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.secret {
            secret.zeroize();
        }
    }
}

/// Only invoke for the exact `--credential-worker` argument, before initializing the GUI.
/// A parent deadline terminates this process if an OS provider never replies.
pub fn run_credential_worker() -> Result<(), CredentialError> {
    worker_io(
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &SystemCredentialStore::new(),
    )
}

fn worker_io(
    input: &mut impl Read,
    output: &mut impl Write,
    store: &impl CredentialStore,
) -> Result<(), CredentialError> {
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take((MAX_WIRE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialError::Unavailable)?;
    if bytes.len() > MAX_WIRE_BYTES {
        return Err(CredentialError::Unavailable);
    }
    let mut request: Request =
        serde_json::from_slice(&bytes).map_err(|_| CredentialError::Unavailable)?;
    if request.reference.0.is_nil() {
        return Err(CredentialError::InvalidReference);
    }
    let result = match request.operation {
        Operation::Get if request.secret.is_none() => {
            store.get(request.reference).and_then(|secret| {
                if secret.expose().len() > MAX_SECRET_BYTES {
                    return Err(CredentialError::Unavailable);
                }
                Ok(Some(secret.expose().to_owned()))
            })
        }
        Operation::Set
            if request
                .secret
                .as_ref()
                .is_some_and(|secret| secret.len() <= MAX_SECRET_BYTES) =>
        {
            let secret = Secret::new(request.secret.take().ok_or(CredentialError::Unavailable)?);
            store.set(request.reference, &secret).map(|()| None)
        }
        Operation::Delete if request.secret.is_none() => {
            store.delete(request.reference).map(|()| None)
        }
        _ => return Err(CredentialError::Unavailable),
    };
    let reply = match result {
        Ok(secret) => Reply {
            status: Status::Ok,
            secret,
        },
        Err(error) => Reply {
            status: match error {
                CredentialError::Missing => Status::Missing,
                CredentialError::InvalidReference => Status::InvalidReference,
                CredentialError::OutcomeUnknown => Status::OutcomeUnknown,
                // A lost D-Bus reply does not prove that a write was rejected.
                CredentialError::Unavailable if request.operation.mutates() => {
                    Status::OutcomeUnknown
                }
                CredentialError::Unavailable => Status::Unavailable,
            },
            secret: None,
        },
    };
    let bytes =
        Zeroizing::new(serde_json::to_vec(&reply).map_err(|_| CredentialError::Unavailable)?);
    output
        .write_all(&bytes)
        .and_then(|()| output.flush())
        .map_err(|_| CredentialError::Unavailable)
}

#[cfg(unix)]
fn exchange(
    command: &mut std::process::Command,
    wire: &[u8],
    timeout: Duration,
    mutates: bool,
) -> Result<Reply, CredentialError> {
    use std::{
        io::ErrorKind,
        process::{Child, Stdio},
        time::Instant,
    };
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            // kill may report InvalidInput after try_wait reaped an exited worker; harmless.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn nonblocking(fd: &impl std::os::fd::AsFd) -> Result<(), CredentialError> {
        let flags = rustix::fs::fcntl_getfl(fd).map_err(|_| CredentialError::Unavailable)?;
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|_| CredentialError::Unavailable)
    }
    let started = Instant::now();
    let mut child = Reap(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| CredentialError::Unavailable)?,
    );
    let mut input = Some(child.0.stdin.take().ok_or(CredentialError::Unavailable)?);
    let mut output = child.0.stdout.take().ok_or(CredentialError::Unavailable)?;
    nonblocking(input.as_ref().ok_or(CredentialError::Unavailable)?)?;
    nonblocking(&output)?;
    let mut sent = 0;
    let mut received = Zeroizing::new(Vec::new());
    let mut buffer = Zeroizing::new([0u8; 8192]);
    let uncertain = |sent: usize| {
        if mutates && sent == wire.len() {
            CredentialError::OutcomeUnknown
        } else {
            CredentialError::Unavailable
        }
    };
    loop {
        if started.elapsed() >= timeout {
            return Err(uncertain(sent));
        }
        if let Some(pipe) = &mut input {
            match pipe.write(&wire[sent..]) {
                Ok(0) => return Err(uncertain(sent)),
                Ok(count) => sent += count,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => return Err(uncertain(sent)),
            }
            if sent == wire.len() {
                input.take();
            }
        }
        let mut eof = false;
        loop {
            match output.read(buffer.as_mut()) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => {
                    if received.len() + count > MAX_WIRE_BYTES {
                        return Err(uncertain(sent));
                    }
                    received.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err(uncertain(sent)),
            }
        }
        if let Some(status) = child.0.try_wait().map_err(|_| uncertain(sent))? {
            if !status.success() {
                return Err(uncertain(sent));
            }
            if eof {
                let reply: Reply =
                    serde_json::from_slice(&received).map_err(|_| uncertain(sent))?;
                if reply
                    .secret
                    .as_ref()
                    .is_some_and(|secret| secret.len() > MAX_SECRET_BYTES)
                {
                    return Err(uncertain(sent));
                }
                return Ok(reply);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn exchange(
    command: &mut std::process::Command,
    wire: &[u8],
    timeout: Duration,
    mutates: bool,
) -> Result<Reply, CredentialError> {
    use std::{
        os::windows::process::CommandExt,
        process::{Child, Stdio},
        sync::mpsc,
        time::Instant,
    };
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Reap(
        command
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| CredentialError::Unavailable)?,
    );
    let mut input = child.0.stdin.take().ok_or(CredentialError::Unavailable)?;
    let output = child.0.stdout.take().ok_or(CredentialError::Unavailable)?;
    let wire = Zeroizing::new(wire.to_vec());
    let (send, receive) = mpsc::sync_channel(1);
    let writer = std::thread::spawn(move || input.write_all(&wire));
    let reader = std::thread::spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let result = output
            .take((MAX_WIRE_BYTES + 1) as u64)
            .read_to_end(&mut bytes);
        let _ = send.send(result.map(|_| bytes));
    });
    let started = Instant::now();
    let result = loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break Err(if mutates {
                CredentialError::OutcomeUnknown
            } else {
                CredentialError::Unavailable
            });
        }
        match receive.recv_timeout(remaining.min(Duration::from_millis(20))) {
            Ok(Ok(bytes)) if bytes.len() <= MAX_WIRE_BYTES => {
                break serde_json::from_slice(&bytes).map_err(|_| CredentialError::Unavailable);
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(CredentialError::Unavailable);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    // The worker never spawns descendants; terminating it closes both anonymous
    // pipes and releases blocked reader/writer threads before returning.
    drop(child);
    let _ = writer.join();
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeStore;
    impl CredentialStore for FakeStore {
        fn get(&self, _: CredentialRef) -> Result<Secret, CredentialError> {
            Ok(Secret::new("opaque\n密碼\0".into()))
        }
        fn set(&self, _: CredentialRef, _: &Secret) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
        fn delete(&self, _: CredentialRef) -> Result<(), CredentialError> {
            Ok(())
        }
    }
    #[test]
    fn worker_keeps_opaque_secrets_and_reports_uncertain_writes() {
        let mut request = Request {
            operation: Operation::Get,
            reference: CredentialRef::new(),
            secret: None,
        };
        let mut output = Vec::new();
        worker_io(
            &mut serde_json::to_vec(&request).unwrap().as_slice(),
            &mut output,
            &FakeStore,
        )
        .unwrap();
        let reply: Reply = serde_json::from_slice(&output).unwrap();
        assert!(reply.result().is_ok());
        assert_eq!(reply.secret.as_deref(), Some("opaque\n密碼\0"));
        request.operation = Operation::Set;
        request.secret = Some("test-only".into());
        output.clear();
        worker_io(
            &mut serde_json::to_vec(&request).unwrap().as_slice(),
            &mut output,
            &FakeStore,
        )
        .unwrap();
        let reply: Reply = serde_json::from_slice(&output).unwrap();
        assert!(reply.result().unwrap_err().may_have_committed());
    }
    #[test]
    fn malformed_worker_input_never_echoes_source() {
        let mut output = Vec::new();
        let error = worker_io(
            &mut b"{\"secret\":\"do-not-echo\"}".as_slice(),
            &mut output,
            &FakeStore,
        )
        .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("do-not-echo"));
        assert!(output.is_empty());
    }
    #[test]
    fn hung_worker_is_terminated_with_a_bounded_wait() {
        #[cfg(unix)]
        let mut command = std::process::Command::new("/bin/sleep");
        #[cfg(unix)]
        command.arg("5");
        #[cfg(windows)]
        let mut command = std::process::Command::new("powershell.exe");
        #[cfg(windows)]
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 5",
        ]);
        let start = std::time::Instant::now();
        let result = exchange(&mut command, b"{}", Duration::from_millis(40), true);
        assert!(matches!(result, Err(CredentialError::OutcomeUnknown)));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
