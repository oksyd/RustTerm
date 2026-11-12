//! Bounded shell fixtures, independent of the developer's terminal and environment.

use std::{
    io::{self, Read, Seek, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Output},
    time::{Duration, Instant},
};

pub(super) fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("TERM", "dumb");
    // SAFETY: setsid is async-signal-safe and the closure accesses no shared state.
    // Without a new session, interactive Bash can send SIGTTIN to Nextest's
    // background process group and suspend the test itself along with the shell.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

struct ShellProcess(Option<Child>);

impl Drop for ShellProcess {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // SAFETY: this unreaped child owns the session/process group created
            // by setsid above. Killing its group also cleans up waiting children.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
        }
    }
}

pub(super) fn output(command: &mut Command, input: &[u8], timeout: Duration) -> io::Result<Output> {
    // Files avoid blocking on a full pipe, including when descendants inherit
    // standard streams. The input is still consumed by the real shell bootstrap.
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    stdin.rewind()?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut process = ShellProcess(Some(
        command
            .stdin(stdin)
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            .spawn()?,
    ));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = process.0.as_mut().unwrap().try_wait()? {
            // try_wait reaped the child; never signal its potentially reusable ID.
            process.0.take();
            break Some(status);
        }
        if start.elapsed() >= timeout {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(process);
    let mut captured_stdout = Vec::new();
    let mut captured_stderr = Vec::new();
    stdout.rewind()?;
    stdout.read_to_end(&mut captured_stdout)?;
    stderr.rewind()?;
    stderr.read_to_end(&mut captured_stderr)?;
    let Some(status) = status else {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "shell test exceeded {timeout:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&captured_stdout),
                String::from_utf8_lossy(&captured_stderr),
            ),
        ));
    };
    Ok(Output {
        status,
        stdout: captured_stdout,
        stderr: captured_stderr,
    })
}

#[test]
fn stopped_shell_times_out_with_captured_diagnostics() {
    let start = Instant::now();
    let error = output(
        command("/bin/sh").args(["-c", "printf 'before stop' >&2; kill -STOP $$"]),
        b"",
        Duration::from_millis(500),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("before stop"));
    assert!(start.elapsed() < Duration::from_secs(5));
}
