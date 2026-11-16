use super::{portable, *};
use std::{
    ffi::OsString,
    fs,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
};

const HELPER: &str = "--apply-portable-update";

pub(super) fn launch(prepared: &portable::Prepared, config_dir: Option<&Path>) -> Result<()> {
    let args = config_dir.map_or_else(Vec::new, |path| {
        vec![OsString::from("--config-dir"), path.as_os_str().to_owned()]
    });
    launch_directory(prepared.directory.path(), &args)
}

fn launch_directory(directory: &Path, args: &[OsString]) -> Result<()> {
    let directory = directory.canonicalize()?;
    let helper = directory.join("helper.exe");
    let plan = portable::plan(&directory)?;
    ensure!(
        portable::hash(&helper)? == plan.before[portable::BINARY],
        "Update helper checksum failed"
    );
    let log = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(directory.join("helper.log"))?;
    let mut child = Command::new(&helper)
        .arg(HELPER)
        .arg(&directory)
        .arg(std::process::id().to_string())
        .arg("--")
        .args(args)
        .current_dir(directory.parent().context("No installation directory")?)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .context("Could not start the update helper")?;
    let mut stdout = child
        .stdout
        .take()
        .context("Missing update helper handshake")?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut ready = [0u8; 6];
        let result = stdout.read_exact(&mut ready).map(|()| ready == *b"ready\n");
        let _ = send.send(result);
    });
    if !matches!(receive.recv_timeout(Duration::from_secs(15)), Ok(Ok(true))) {
        let _ = child.kill();
        let _ = child.wait();
        bail!("The update helper could not prepare to restart RustTerm");
    }
    Ok(())
}

fn process_handle(pid: u32) -> Result<OwnedHandle> {
    // SAFETY: request only synchronization access; the returned handle is owned
    // until waiting completes, so PID reuse cannot redirect the wait.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("Could not wait for the running RustTerm process");
    }
    // SAFETY: OpenProcess returned a valid handle owned by this function.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn wait_for_exit(handle: &OwnedHandle, timeout: u32) -> Result<()> {
    // SAFETY: the owned process handle remains alive for the entire wait.
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), timeout) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => bail!("RustTerm did not exit in time; the update was not installed"),
        _ => Err(std::io::Error::last_os_error()).context("Could not wait for RustTerm to exit"),
    }
}

/// Handle the private updater entry before creating a GUI or acquiring profile locks.
pub fn run_update_helper() -> Result<bool> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER)) {
        return Ok(false);
    }
    let directory =
        PathBuf::from(args.next().context("Missing update directory")?).canonicalize()?;
    let pid: u32 = args
        .next()
        .context("Missing parent process")?
        .to_str()
        .context("Invalid parent process")?
        .parse()?;
    ensure!(
        args.next().as_deref() == Some(std::ffi::OsStr::new("--")),
        "Invalid update helper arguments"
    );
    let args: Vec<_> = args.collect();
    ensure!(
        std::env::current_exe()?.canonicalize()? == directory.join("helper.exe"),
        "Update helper must run from its staging directory"
    );
    let plan = portable::plan(&directory)?;
    ensure!(
        portable::hash(&directory.join("helper.exe"))? == plan.before[portable::BINARY],
        "Update helper checksum failed"
    );
    let parent = process_handle(pid)?;
    fs::write(directory.join("helper.pid"), std::process::id().to_string())?;
    std::io::stdout().write_all(b"ready\n")?;
    std::io::stdout().flush()?;
    if let Err(error) = wait_for_exit(&parent, 60_000) {
        show_error(&error);
        return Err(error);
    }
    let prefix = directory.parent().context("No installation directory")?;
    let result = (|| -> Result<()> {
        let _lock = portable::lock(prefix)?;
        portable::apply(&directory)?;
        if let Err(error) = restart(prefix, &args) {
            portable::undo(&directory).context(
                "The new version could not start and rollback failed; backups were preserved",
            )?;
            return Err(error);
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("{error:#}");
        show_error(&error);
        if portable::validate_installation(prefix, CURRENT_VERSION).is_ok() {
            let _ = restart(prefix, &args);
        }
        return Err(error);
    }
    Ok(true)
}

fn restart(prefix: &Path, args: &[OsString]) -> Result<()> {
    Command::new(prefix.join(portable::BINARY))
        .args(args)
        .current_dir(prefix)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not start the updated RustTerm")?;
    Ok(())
}

fn show_error(error: &anyhow::Error) {
    let text: Vec<u16> = format!("RustTerm update failed:\n\n{error:#}")
        .replace('\0', "")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = "RustTerm\0".encode_utf16().collect();
    // SAFETY: both strings are terminated UTF-16 buffers valid through the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn update_directories() -> Result<Vec<PathBuf>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    if executable
        .file_name()
        .is_none_or(|name| name != portable::BINARY)
    {
        return Ok(Vec::new());
    }
    let prefix = executable.parent().context("No executable directory")?;
    Ok(fs::read_dir(prefix)?
        .take(1024)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            (entry.file_name().to_str()?.starts_with(portable::PREFIX)
                && portable::regular(&path, true).is_ok())
            .then_some(path)
        })
        .collect())
}

/// Resume a transaction interrupted after replacement started, before loading
/// profile data. The helper waits for this recovery launcher to exit as usual.
pub fn recover_interrupted_update() -> Result<bool> {
    for directory in update_directories()? {
        if directory.join("applying").try_exists()? && !directory.join("complete").try_exists()? {
            let _lock = portable::lock(directory.parent().context("No installation directory")?)?;
            launch_directory(&directory, &std::env::args_os().skip(1).collect::<Vec<_>>())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// The helper's executable is locked until it exits. Clean completed staging
/// folders asynchronously, after validating that their transaction was committed.
pub fn cleanup_completed_updates() {
    let Ok(directories) = update_directories() else {
        return;
    };
    if directories.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for directory in directories {
            if !directory.join("complete").is_file() {
                continue;
            }
            let clean = || -> Result<()> {
                ensure!(
                    directory.join("complete").try_exists()?,
                    "Update is not complete"
                );
                let prefix = directory.parent().context("No installation directory")?;
                let _lock = portable::lock(prefix)?;
                let plan = portable::plan(&directory)?;
                let pid: u32 = fs::read_to_string(directory.join("helper.pid"))?.parse()?;
                match process_handle(pid) {
                    Ok(helper) => wait_for_exit(&helper, 0)?,
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .and_then(|e| e.raw_os_error())
                            == Some(87) => {}
                    Err(error) => return Err(error),
                }
                for name in portable::FILES {
                    ensure!(
                        portable::hash(&prefix.join(name))? == plan.after[*name],
                        "Update is not committed"
                    );
                }
                fs::remove_dir_all(&directory)?;
                Ok(())
            };
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(500));
                if clean().is_ok() {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "child process fixture invoked by parent_wait_observes_exit"]
    fn process_fixture() {
        std::io::stdin().read_exact(&mut [0u8]).unwrap();
    }

    #[test]
    fn parent_wait_observes_exit() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "update::windows::tests::process_fixture",
                "--ignored",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let handle = process_handle(child.id()).unwrap();
        assert!(wait_for_exit(&handle, 0).is_err());
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        wait_for_exit(&handle, 10_000).unwrap();
        assert!(child.wait().unwrap().success());
    }
}
