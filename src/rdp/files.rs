//! Opened, read-only sources for clipboard file offers. Protocol code sees metadata only.

use super::{RdpError, Result};
use rustrdp::{ClipboardFile, ClipboardFileRequest, MAX_CLIPBOARD_FILE_CHUNK, MAX_CLIPBOARD_FILES};
use std::{
    collections::BTreeSet,
    fs::{File, Metadata, OpenOptions},
    io,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
use tokio_util::sync::CancellationToken;

const READ_CHUNK: usize = 64 * 1024;

pub(super) struct OpenFiles {
    sources: Vec<Source>,
}

struct Source {
    file: Option<File>,
    description: ClipboardFile,
    stamp: Stamp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            size: metadata.len(),
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

impl OpenFiles {
    /// Opening and reading belong on a blocking worker, never the render loop.
    pub(super) fn open(paths: Vec<PathBuf>, cancel: CancellationToken) -> Result<Self> {
        check_cancelled(&cancel)?;
        if paths.is_empty() || paths.len() > MAX_CLIPBOARD_FILES {
            return Err(RdpError::FileSelection);
        }
        let mut names = BTreeSet::new();
        let mut sources = Vec::with_capacity(paths.len());
        for path in paths {
            check_cancelled(&cancel)?;
            if !path.is_absolute() {
                return Err(RdpError::FileSelection);
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(RdpError::FileSelection)?;
            if !valid_name(name) || names.contains(&name.to_lowercase()) {
                return Err(RdpError::FileSelection);
            }
            // O_NONBLOCK prevents a selected FIFO from blocking before fstat;
            // O_NOFOLLOW prevents a final symlink from changing the chosen source.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .map_err(|_| RdpError::FileSelection)?;
            collect(file, name.to_owned(), &mut sources, &mut names, &cancel)?;
        }
        check_cancelled(&cancel)?;
        Ok(Self { sources })
    }

    pub(super) fn descriptions(&self) -> Vec<ClipboardFile> {
        self.sources
            .iter()
            .map(|source| source.description.clone())
            .collect()
    }

    pub(super) fn file_count(&self) -> usize {
        self.sources.len()
    }

    /// Read from the retained file descriptor, never by reopening a clipboard path.
    pub(super) fn read(
        &self,
        request: &ClipboardFileRequest,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        check_cancelled(cancel)?;
        let source = self
            .sources
            .get(request.index as usize)
            .ok_or(RdpError::FileUnavailable)?;
        let file = source.file.as_ref().ok_or(RdpError::FileUnavailable)?;
        if request.length > MAX_CLIPBOARD_FILE_CHUNK
            || request
                .offset
                .checked_add(u64::from(request.length))
                .is_none_or(|end| end > source.stamp.size)
        {
            return Err(RdpError::FileUnavailable);
        }
        source.verify()?;
        let mut data = vec![0; request.length as usize];
        let mut copied = 0;
        while copied < data.len() {
            check_cancelled(cancel)?;
            let end = data.len().min(copied + READ_CHUNK);
            match file.read_at(&mut data[copied..end], request.offset + copied as u64) {
                Ok(0) => return Err(RdpError::FileChanged),
                Ok(count) => copied += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(RdpError::FileUnavailable),
            }
        }
        source.verify()?;
        check_cancelled(cancel)?;
        Ok(data)
    }
}

fn collect(
    file: File,
    name: String,
    sources: &mut Vec<Source>,
    names: &mut BTreeSet<String>,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    if sources.len() == MAX_CLIPBOARD_FILES
        || name.encode_utf16().count() > 259
        || !names.insert(name.to_lowercase())
    {
        return Err(RdpError::FileSelection);
    }
    let metadata = file.metadata().map_err(|_| RdpError::FileUnavailable)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(RdpError::FileSelection);
    }
    let stamp = Stamp::from_metadata(&metadata);
    let description = ClipboardFile {
        name: name.clone(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        directory: metadata.is_dir(),
    };
    if !metadata.is_dir() {
        sources.push(Source {
            file: Some(file),
            description,
            stamp,
        });
        return Ok(());
    }
    sources.push(Source {
        file: None,
        description,
        stamp,
    });
    let directory = rustix::fs::Dir::read_from(&file).map_err(|_| RdpError::FileUnavailable)?;
    let mut children = Vec::new();
    for entry in directory {
        check_cancelled(cancel)?;
        let entry = entry.map_err(|_| RdpError::FileUnavailable)?;
        let child = entry
            .file_name()
            .to_str()
            .map_err(|_| RdpError::FileSelection)?;
        if matches!(child, "." | "..") {
            continue;
        }
        if !valid_name(child) || children.len() >= MAX_CLIPBOARD_FILES - sources.len() {
            return Err(RdpError::FileSelection);
        }
        children.push(child.to_owned());
    }
    children.sort_unstable();
    for child in children {
        check_cancelled(cancel)?;
        // Relative descriptor traversal cannot follow a replaced directory or symlink.
        let fd = rustix::fs::openat(
            &file,
            child.as_str(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| RdpError::FileSelection)?;
        collect(
            fd.into(),
            format!("{name}\\{child}"),
            sources,
            names,
            cancel,
        )?;
    }
    if Stamp::from_metadata(&file.metadata().map_err(|_| RdpError::FileUnavailable)?) != stamp {
        return Err(RdpError::FileChanged);
    }
    Ok(())
}

impl Source {
    fn verify(&self) -> Result<()> {
        let metadata = self
            .file
            .as_ref()
            .ok_or(RdpError::FileUnavailable)?
            .metadata()
            .map_err(|_| RdpError::FileUnavailable)?;
        if !metadata.is_file() || Stamp::from_metadata(&metadata) != self.stamp {
            return Err(RdpError::FileChanged);
        }
        Ok(())
    }
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(RdpError::FileCancelled)
    } else {
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.ends_with([' ', '.'])
        || name
            .chars()
            .any(|ch| ch < ' ' || "<>:\"/\\|?*".contains(ch))
        || name.encode_utf16().count() > 259
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !((stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(
                stem.get(3..),
                Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        fs::FileTimes,
        os::unix::fs::symlink,
        time::{Duration, UNIX_EPOCH},
    };

    fn request(index: u32, offset: u64, length: u32) -> ClipboardFileRequest {
        ClipboardFileRequest {
            id: 1,
            offer_id: 7,
            index,
            offset,
            length,
        }
    }

    #[test]
    fn directory_offers_include_empty_children_and_retained_regular_sources() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("folder");
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("data.txt"), b"abc").unwrap();
        let source = OpenFiles::open(vec![root.clone()], CancellationToken::new()).unwrap();
        let entries = source.descriptions();
        assert!(entries[0].directory);
        let index = entries
            .iter()
            .position(|entry| entry.name == "folder\\data.txt")
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == "folder\\empty" && entry.directory)
        );
        assert_eq!(
            source
                .read(&request(index as u32, 0, 3), &CancellationToken::new())
                .unwrap(),
            b"abc"
        );
        assert_eq!(
            source.read(&request(0, 0, 0), &CancellationToken::new()),
            Err(RdpError::FileUnavailable)
        );
        symlink(parent.path(), root.join("loop")).unwrap();
        assert!(matches!(
            OpenFiles::open(vec![root], CancellationToken::new()),
            Err(RdpError::FileSelection)
        ));
    }

    #[test]
    fn retained_sources_return_bounded_ranges_and_basename_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report 中文.txt");
        fs::write(&path, b"abcdefghij").unwrap();
        let sources = OpenFiles::open(vec![path.clone()], CancellationToken::new()).unwrap();
        assert_eq!(sources.file_count(), 1);
        assert_eq!(
            sources.descriptions(),
            vec![ClipboardFile {
                directory: false,
                name: "report 中文.txt".into(),
                size: 10
            }]
        );
        let cancel = CancellationToken::new();
        assert_eq!(sources.read(&request(0, 3, 4), &cancel).unwrap(), b"defg");
        assert_eq!(sources.read(&request(0, 10, 0), &cancel).unwrap(), b"");
        assert_eq!(
            sources.read(&request(0, 9, 2), &cancel),
            Err(RdpError::FileUnavailable)
        );
        assert_eq!(
            sources.read(&request(1, 0, 1), &cancel),
            Err(RdpError::FileUnavailable)
        );
        assert_eq!(
            sources.read(&request(0, u64::MAX, 1), &cancel),
            Err(RdpError::FileUnavailable)
        );
    }

    #[test]
    fn truncation_and_same_size_modification_reject_a_stale_offer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        fs::write(&path, b"1234").unwrap();
        let sources = OpenFiles::open(vec![path.clone()], CancellationToken::new()).unwrap();
        fs::write(&path, b"1").unwrap();
        assert_eq!(
            sources.read(&request(0, 0, 4), &CancellationToken::new()),
            Err(RdpError::FileChanged)
        );
        fs::write(&path, b"1234").unwrap();
        let sources = OpenFiles::open(vec![path.clone()], CancellationToken::new()).unwrap();
        fs::write(&path, b"abcd").unwrap();
        File::open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            sources.read(&request(0, 0, 4), &CancellationToken::new()),
            Err(RdpError::FileChanged)
        );
    }

    #[test]
    fn symlinks_devices_and_ambiguous_names_are_rejected_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("target");
        fs::write(&path, b"data").unwrap();
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        for path in [
            link,
            directory.path().to_owned(),
            PathBuf::from("/dev/null"),
        ] {
            assert!(matches!(
                OpenFiles::open(vec![path], CancellationToken::new()),
                Err(RdpError::FileSelection)
            ));
        }
        let upper = directory.path().join("A.txt");
        let lower = directory.path().join("a.txt");
        fs::write(&upper, b"").unwrap();
        fs::write(&lower, b"").unwrap();
        assert!(matches!(
            OpenFiles::open(vec![upper, lower], CancellationToken::new()),
            Err(RdpError::FileSelection)
        ));
        for name in ["CON", "COM1.log", "name.", "bad:name", "bad\nname"] {
            assert!(matches!(
                OpenFiles::open(vec![directory.path().join(name)], CancellationToken::new()),
                Err(RdpError::FileSelection)
            ));
        }
    }

    #[test]
    fn cancellation_prevents_open_and_read_work() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            OpenFiles::open(vec![], cancel.clone()),
            Err(RdpError::FileCancelled)
        ));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        fs::write(&path, [0; 128]).unwrap();
        let sources = OpenFiles::open(vec![path], CancellationToken::new()).unwrap();
        assert_eq!(
            sources.read(&request(0, 0, 128), &cancel),
            Err(RdpError::FileCancelled)
        );
    }
}
