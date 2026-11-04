//! Sequential, bounded staging of remote clipboard files away from the protocol loop.

use super::{RdpError, Result};
use rustrdp::{ClipboardFile, ClipboardFileRequest, MAX_CLIPBOARD_FILES};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const CHUNK_BYTES: u32 = 256 * 1024;
const WRITE_BYTES: usize = 64 * 1024;

pub(super) enum Next {
    Request {
        index: u32,
        offset: u64,
        length: u32,
    },
    Ready {
        directory: Arc<TempDir>,
        paths: Vec<PathBuf>,
    },
}

pub(super) struct Completed {
    pub offer_id: u64,
    pub bytes_received: u64,
    pub total_bytes: u64,
    pub result: Result<Next>,
}

struct Active {
    generation: u64,
    offer_id: u64,
    cancel: CancellationToken,
    pending: Option<Vec<ClipboardFile>>,
    staging: Option<Staging>,
    received: u64,
    total: u64,
}

pub(super) struct Downloads {
    jobs: JoinSet<Result<Option<Staging>>>,
    running: Option<u64>,
    generation: u64,
    active: Option<Active>,
}

impl Downloads {
    pub fn new() -> Self {
        Self {
            jobs: JoinSet::new(),
            running: None,
            generation: 0,
            active: None,
        }
    }

    pub fn active_offer(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.offer_id)
    }

    pub fn is_busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub fn begin(&mut self, offer_id: u64, files: Vec<ClipboardFile>) -> Result<()> {
        let total = validate_metadata(&files)?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(RdpError::Limit("clipboard download generation"))?;
        self.cancel();
        self.generation = generation;
        self.active = Some(Active {
            generation,
            offer_id,
            cancel: CancellationToken::new(),
            pending: Some(files),
            staging: None,
            received: 0,
            total,
        });
        self.start_pending();
        Ok(())
    }

    pub fn accept(&mut self, request: ClipboardFileRequest, data: Vec<u8>) -> Result<()> {
        let active = self.active.as_mut().ok_or(RdpError::FileCancelled)?;
        if active.offer_id != request.offer_id || active.cancel.is_cancelled() {
            return Err(RdpError::FileCancelled);
        }
        let staging = active.staging.as_ref().ok_or(RdpError::FileDownload)?;
        staging.validate_response(&request, data.len())?;
        let staging = active.staging.take().ok_or(RdpError::FileDownload)?;
        self.running = Some(active.generation);
        self.jobs
            .spawn_blocking(move || staging.write(request, data).map(Some));
        Ok(())
    }

    pub fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.cancel();
            if let Some(staging) = active.staging {
                self.clean(active.generation, staging);
            }
        }
    }

    pub async fn next(&mut self) -> Option<Completed> {
        loop {
            let joined = self.jobs.join_next().await?;
            let generation = self.running.take()?;
            let result = joined.unwrap_or(Err(RdpError::FileDownload));
            if self
                .active
                .as_ref()
                .is_none_or(|active| active.generation != generation)
            {
                if let Ok(Some(staging)) = result {
                    self.clean(generation, staging);
                }
                self.start_pending();
                continue;
            }
            let active = self.active.as_mut()?;
            let result = match result {
                Ok(Some(staging)) => {
                    active.received = staging.received;
                    match staging.request() {
                        Some((index, offset, length)) => {
                            active.staging = Some(staging);
                            Ok(Next::Request {
                                index,
                                offset,
                                length,
                            })
                        }
                        None => Ok(Next::Ready {
                            paths: staging.paths,
                            directory: Arc::new(staging.directory),
                        }),
                    }
                }
                Ok(None) => Err(RdpError::FileCancelled),
                Err(error) => Err(error),
            };
            let completed = Completed {
                offer_id: active.offer_id,
                bytes_received: active.received,
                total_bytes: active.total,
                result,
            };
            if !matches!(&completed.result, Ok(Next::Request { .. })) {
                self.active.take();
            }
            return Some(completed);
        }
    }

    fn start_pending(&mut self) {
        if !self.jobs.is_empty() {
            return;
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(files) = active.pending.take() else {
            return;
        };
        let cancel = active.cancel.clone();
        self.running = Some(active.generation);
        self.jobs.spawn_blocking(move || {
            check_cancelled(&cancel)?;
            let parent = cache_directory()?;
            Staging::prepare(&parent, files, cancel).map(Some)
        });
    }

    fn clean(&mut self, generation: u64, staging: Staging) {
        self.running = Some(generation);
        self.jobs.spawn_blocking(move || {
            drop(staging);
            Ok(None)
        });
    }
}

impl Drop for Downloads {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.cancel();
        }
    }
}

struct Staging {
    directory: TempDir,
    root: File,
    files: Vec<ClipboardFile>,
    paths: Vec<PathBuf>,
    file: Option<File>,
    index: usize,
    offset: u64,
    received: u64,
    cancel: CancellationToken,
}

impl Staging {
    fn prepare(
        parent: &Path,
        files: Vec<ClipboardFile>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        check_cancelled(&cancel)?;
        validate_metadata(&files)?;
        let directory = tempfile::Builder::new()
            .prefix("download-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .map_err(file_error)?;
        let space = rustix::fs::statvfs(directory.path()).map_err(|_| RdpError::FileDownload)?;
        let block = space.f_frsize.max(1);
        let available = u128::from(space.f_bavail) * u128::from(block);
        let required: u128 = files
            .iter()
            .map(|file| u128::from(file.size).div_ceil(u128::from(block)) * u128::from(block))
            .sum();
        if required > available {
            return Err(RdpError::FileSpace);
        }
        let root = File::open(directory.path()).map_err(file_error)?;
        let paths = files
            .iter()
            .map(|file| file.name.split('\\').next().unwrap())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|name| directory.path().join(name))
            .collect();
        let mut staging = Self {
            directory,
            root,
            files,
            paths,
            file: None,
            index: 0,
            offset: 0,
            received: 0,
            cancel,
        };
        staging.advance()?;
        Ok(staging)
    }

    fn advance(&mut self) -> Result<()> {
        self.file = None;
        while let Some(description) = self.files.get(self.index) {
            check_cancelled(&self.cancel)?;
            let mut parts: Vec<_> = description.name.split('\\').collect();
            let leaf = parts.pop().ok_or(RdpError::FileSelection)?;
            if description.directory {
                parts.push(leaf);
                create_directory(&self.root, &parts)?;
                self.index += 1;
                continue;
            }
            let parent = create_directory(&self.root, &parts)?;
            let file: File = rustix::fs::openat(
                &parent,
                leaf,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(|error| file_error(error.into()))?
            .into();
            if description.size != 0 {
                self.file = Some(file);
                return Ok(());
            }
            self.index += 1;
        }
        check_cancelled(&self.cancel)
    }

    fn request(&self) -> Option<(u32, u64, u32)> {
        self.files.get(self.index).map(|file| {
            (
                self.index as u32,
                self.offset,
                (file.size - self.offset).min(u64::from(CHUNK_BYTES)) as u32,
            )
        })
    }

    fn validate_response(&self, request: &ClipboardFileRequest, length: usize) -> Result<()> {
        check_cancelled(&self.cancel)?;
        if self.request() != Some((request.index, request.offset, request.length))
            || length != request.length as usize
            || length > CHUNK_BYTES as usize
        {
            return Err(RdpError::FileDownload);
        }
        Ok(())
    }

    fn write(mut self, request: ClipboardFileRequest, data: Vec<u8>) -> Result<Self> {
        self.validate_response(&request, data.len())?;
        let file = self.file.as_mut().ok_or(RdpError::FileDownload)?;
        let mut written = 0;
        while written < data.len() {
            check_cancelled(&self.cancel)?;
            let end = data.len().min(written + WRITE_BYTES);
            match file.write(&data[written..end]) {
                Ok(0) => return Err(RdpError::FileDownload),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(file_error(error)),
            }
        }
        self.offset += written as u64;
        self.received += written as u64;
        if self.offset == self.files[self.index].size {
            self.index += 1;
            self.offset = 0;
            self.advance()?;
        }
        check_cancelled(&self.cancel)?;
        Ok(self)
    }
}

fn create_directory(root: &File, parts: &[&str]) -> Result<File> {
    let mut parent = root.try_clone().map_err(file_error)?;
    for part in parts {
        match rustix::fs::mkdirat(&parent, *part, rustix::fs::Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(file_error(error.into())),
        }
        parent = rustix::fs::openat(
            &parent,
            *part,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| file_error(error.into()))?
        .into();
    }
    Ok(parent)
}

fn validate_metadata(files: &[ClipboardFile]) -> Result<u64> {
    if files.is_empty() || files.len() > MAX_CLIPBOARD_FILES {
        return Err(RdpError::FileSelection);
    }
    let mut names = BTreeMap::new();
    let mut total = 0u64;
    for file in files {
        if file.name.encode_utf16().count() > 259
            || file.size > i64::MAX as u64
            || (file.directory && file.size != 0)
            || names.insert(file.name.to_lowercase(), file).is_some()
        {
            return Err(RdpError::FileSelection);
        }
        for part in file.name.split('\\') {
            if part.is_empty()
                || part.len() > 255
                || matches!(part, "." | "..")
                || part.ends_with([' ', '.'])
                || part
                    .chars()
                    .any(|ch| ch.is_control() || "<>:\"/|?*".contains(ch))
            {
                return Err(RdpError::FileSelection);
            }
        }
        total = total
            .checked_add(file.size)
            .ok_or(RdpError::FileSelection)?;
    }
    let mut ancestors = BTreeMap::new();
    for file in files {
        for (offset, _) in file.name.match_indices('\\') {
            let parent = &file.name[..offset];
            let key = parent.to_lowercase();
            if names
                .get(&key)
                .is_some_and(|entry| !entry.directory || entry.name != parent)
                || ancestors
                    .insert(key, parent)
                    .is_some_and(|previous| previous != parent)
            {
                return Err(RdpError::FileSelection);
            }
        }
    }
    Ok(total)
}

fn cache_directory() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .filter(|path| path.is_absolute())
        .ok_or(RdpError::FileDownload)?;
    let parent = cache.join("rustterm/rdp-clipboard");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&parent)
        .map_err(file_error)?;
    Ok(parent)
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(RdpError::FileCancelled)
    } else {
        Ok(())
    }
}

fn file_error(error: io::Error) -> RdpError {
    if matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) {
        RdpError::FileSpace
    } else {
        RdpError::FileDownload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn description(name: &str, size: u64) -> ClipboardFile {
        ClipboardFile {
            directory: false,
            name: name.into(),
            size,
        }
    }

    #[test]
    fn directory_tree_stages_privately_and_exports_only_roots() {
        let parent = tempfile::tempdir().unwrap();
        let files = vec![
            ClipboardFile {
                name: "tree".into(),
                size: 0,
                directory: true,
            },
            description("tree\\nested\\data", 3),
            ClipboardFile {
                name: "tree\\empty".into(),
                size: 0,
                directory: true,
            },
        ];
        let mut staging = Staging::prepare(parent.path(), files, CancellationToken::new()).unwrap();
        assert_eq!(staging.request(), Some((1, 0, 3)));
        let request = request(&staging);
        staging = staging.write(request, b"abc".to_vec()).unwrap();
        assert!(staging.request().is_none());
        assert_eq!(staging.paths, vec![staging.directory.path().join("tree")]);
        assert_eq!(
            fs::read(staging.paths[0].join("nested/data")).unwrap(),
            b"abc"
        );
        assert!(staging.paths[0].join("empty").is_dir());
        let path = staging.directory.path().to_owned();
        drop(staging);
        assert!(!path.exists());
        for parent_entry in [
            description("tree", 0),
            ClipboardFile {
                name: "TREE".into(),
                size: 0,
                directory: true,
            },
        ] {
            assert_eq!(
                validate_metadata(&[parent_entry, description("tree\\data", 1)]),
                Err(RdpError::FileSelection)
            );
        }
    }

    fn request(staging: &Staging) -> ClipboardFileRequest {
        let (index, offset, length) = staging.request().unwrap();
        ClipboardFileRequest {
            id: 1,
            offer_id: 7,
            index,
            offset,
            length,
        }
    }

    #[test]
    fn sequential_files_are_private_and_zero_length_files_need_no_request() {
        let parent = tempfile::tempdir().unwrap();
        let mut staging = Staging::prepare(
            parent.path(),
            vec![description("empty", 0), description("中文.txt", 4)],
            CancellationToken::new(),
        )
        .unwrap();
        let directory = staging.directory.path().to_owned();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(staging.request(), Some((1, 0, 4)));
        let requested = request(&staging);
        staging = staging.write(requested, b"test".to_vec()).unwrap();
        assert!(staging.request().is_none());
        assert_eq!(staging.received, 4);
        assert_eq!(fs::read(&staging.paths[0]).unwrap(), b"");
        assert_eq!(fs::read(&staging.paths[1]).unwrap(), b"test");
        for path in &staging.paths {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(staging);
        assert!(!directory.exists());
    }

    #[test]
    fn hostile_names_duplicate_names_and_size_overflow_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "folder\\..\\file",
            "nul\0name",
            "line\nname",
        ] {
            assert_eq!(
                validate_metadata(&[description(name, 0)]),
                Err(RdpError::FileSelection)
            );
        }
        assert_eq!(
            validate_metadata(&[description("A", 1), description("a", 1)]),
            Err(RdpError::FileSelection)
        );
        assert_eq!(
            validate_metadata(&[description("huge", u64::MAX)]),
            Err(RdpError::FileSelection)
        );
        assert_eq!(
            validate_metadata(&[
                description("a", i64::MAX as u64),
                description("b", i64::MAX as u64),
                description("c", 2)
            ]),
            Err(RdpError::FileSelection)
        );
    }

    #[test]
    fn responses_must_match_the_exact_outstanding_bounded_range() {
        let parent = tempfile::tempdir().unwrap();
        let staging = Staging::prepare(
            parent.path(),
            vec![description("data", u64::from(CHUNK_BYTES) + 4)],
            CancellationToken::new(),
        )
        .unwrap();
        let requested = request(&staging);
        assert_eq!(requested.length, CHUNK_BYTES);
        assert_eq!(
            staging.validate_response(&requested, CHUNK_BYTES as usize - 1),
            Err(RdpError::FileDownload)
        );
        assert_eq!(
            staging.validate_response(&requested, CHUNK_BYTES as usize + 1),
            Err(RdpError::FileDownload)
        );
        assert_eq!(
            staging.validate_response(
                &ClipboardFileRequest {
                    offset: 1,
                    ..requested
                },
                CHUNK_BYTES as usize
            ),
            Err(RdpError::FileDownload)
        );
        assert_eq!(
            staging.validate_response(
                &ClipboardFileRequest {
                    index: 1,
                    ..requested
                },
                CHUNK_BYTES as usize
            ),
            Err(RdpError::FileDownload)
        );
        let mut staging = staging
            .write(requested, vec![b'x'; CHUNK_BYTES as usize])
            .unwrap();
        assert_eq!(staging.request(), Some((0, u64::from(CHUNK_BYTES), 4)));
        let requested = request(&staging);
        staging = staging.write(requested, b"tail".to_vec()).unwrap();
        assert!(staging.request().is_none());
        assert_eq!(
            fs::metadata(&staging.paths[0]).unwrap().len(),
            u64::from(CHUNK_BYTES) + 4
        );
    }

    #[test]
    fn cancelled_staging_is_removed_before_any_further_write() {
        let parent = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let staging = Staging::prepare(
            parent.path(),
            vec![description("partial", 4)],
            cancel.clone(),
        )
        .unwrap();
        let directory = staging.directory.path().to_owned();
        let requested = request(&staging);
        cancel.cancel();
        assert!(matches!(
            staging.write(requested, vec![0; 4]),
            Err(RdpError::FileCancelled)
        ));
        assert!(!directory.exists());
        assert_eq!(
            file_error(io::Error::from_raw_os_error(libc::ENOSPC)),
            RdpError::FileSpace
        );
    }
}
