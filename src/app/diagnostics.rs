//! Bounded, asynchronous diagnostics; payloads cannot contain user-supplied strings.
use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{self, SyncSender},
    },
    time::{SystemTime, UNIX_EPOCH},
};

struct Entry {
    directory: PathBuf,
    event: &'static str,
    context: Option<&'static str>,
    values: Vec<u64>,
}

fn enqueue(entry: Entry) {
    static QUEUE: OnceLock<Option<SyncSender<Entry>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Entry>(128);
        std::thread::Builder::new()
            .name("diagnostics".into())
            .spawn(move || {
                for entry in rx {
                    append(entry);
                }
            })
            .ok()
            .map(|_| tx)
    });
    // Diagnostics are expendable. A full or failed log sink never blocks input.
    if let Some(queue) = queue {
        let _ = queue.try_send(entry);
    }
}

pub fn record(directory: &Path, event: &'static str, id: usize) {
    record_values(directory, event, [id as u64, 0, 0]);
}

pub fn record_with_context(
    directory: &Path,
    event: &'static str,
    context: Option<&'static str>,
    id: usize,
) {
    enqueue(Entry {
        directory: directory.to_owned(),
        event,
        context,
        values: vec![id as u64, 0, 0],
    });
}

pub fn record_values(directory: &Path, event: &'static str, values: impl Into<Vec<u64>>) {
    enqueue(Entry {
        directory: directory.to_owned(),
        event,
        context: None,
        values: values.into(),
    });
}

pub fn metrics(directory: &Path, panes: usize, runs: usize, snapshots: u64) {
    enqueue(Entry {
        directory: directory.to_owned(),
        event: "summary-panes-runs-snapshots",
        context: None,
        values: vec![panes as u64, runs as u64, snapshots],
    });
}

fn append(entry: Entry) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // prepare_logs is best effort. Never write through an unsafe state/log
        // directory if preparation failed (or its permissions subsequently changed).
        for directory in [Some(entry.directory.as_path()), entry.directory.parent()]
            .into_iter()
            .flatten()
        {
            let Ok(metadata) = fs::metadata(directory) else {
                return;
            };
            if !metadata.is_dir()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o022 != 0
            {
                return;
            }
        }
    }
    let path = entry.directory.join("diagnostics.log");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && !metadata.is_file()
    {
        return;
    }
    if fs::metadata(&path).is_ok_and(|m| m.len() >= 1024 * 1024) {
        let _ = fs::remove_file(entry.directory.join("diagnostics.2.log"));
        let _ = fs::rename(
            entry.directory.join("diagnostics.1.log"),
            entry.directory.join("diagnostics.2.log"),
        );
        if fs::rename(&path, entry.directory.join("diagnostics.1.log")).is_err() {
            return; // A failed rotation must not turn the log into an unbounded file.
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    if let Ok(mut file) = options.open(path) {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut line = format!(
            "{seconds} {} {} {} values=",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            entry.event,
        );
        for (index, value) in entry.values.iter().enumerate() {
            if index != 0 {
                line.push(',');
            }
            let _ = write!(line, "{value}");
        }
        if let Some(context) = entry.context {
            let _ = write!(line, " context={context:?}");
        }
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}
