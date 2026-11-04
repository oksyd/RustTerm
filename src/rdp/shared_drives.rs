//! File-descriptor confined RDP drive access on one bounded worker per connection.
use super::{RdpError, Result};
use crate::config::RdpSharedFolder;
use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags, ResolveFlags};
use rustrdp::{DriveCompletion, DriveRequest};
use std::{
    collections::HashMap,
    fs::{File, Metadata, ReadDir},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;

const DENIED: u32 = 0xc0000022;
const INVALID: u32 = 0xc000000d;
const NOT_SUPPORTED: u32 = 0xc00000bb;
const NOT_FOUND: u32 = 0xc000000f;
const NO_MORE: u32 = 0x80000006;
const MAX_IO: usize = 1024 * 1024 - 64;
// Match rustrdp's pending device-request limit so every accepted IRP fits,
// including CLOSE, while the worker waits for completion capacity.
const MAX_PENDING_REQUESTS: usize = 256;
type IoResult<T> = std::result::Result<T, u32>;

pub(super) struct SharedDrives {
    requests: mpsc::Sender<DriveRequest>,
    pub completions: mpsc::Receiver<DriveCompletion>,
    stopped: Arc<AtomicBool>,
}
impl SharedDrives {
    pub fn new(config: &[RdpSharedFolder], locked: Arc<AtomicBool>) -> Result<Self> {
        let roots = config
            .iter()
            .map(|folder| {
                let file = File::options()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
                    .open(&folder.path)
                    .map_err(|_| RdpError::FileUnavailable)?;
                if !file
                    .metadata()
                    .map_err(|_| RdpError::FileUnavailable)?
                    .is_dir()
                {
                    return Err(RdpError::FileUnavailable);
                }
                Ok(Root {
                    file,
                    name: folder.name.clone(),
                    read_only: folder.read_only,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (requests, mut rx) = mpsc::channel::<DriveRequest>(MAX_PENDING_REQUESTS);
        let (tx, completions) = mpsc::channel(64);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        std::thread::Builder::new()
            .name("rustterm-rdp-drives".into())
            .spawn(move || {
                let mut worker = Worker {
                    roots,
                    handles: HashMap::new(),
                    next_id: 1,
                    generation: 0,
                };
                while let Some(request) = rx.blocking_recv() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    // Closing handles remains possible while locked; all new accesses are denied.
                    let result = if locked.load(Ordering::Acquire) && request.major != 2 {
                        Err(DENIED)
                    } else {
                        worker.process(&request)
                    };
                    let (status, data) = match result {
                        Ok(data) => (0, data),
                        Err(status) => (status, error_body(request.major)),
                    };
                    if tx
                        .blocking_send(DriveCompletion {
                            generation: request.generation,
                            device_id: request.device_id,
                            completion_id: request.completion_id,
                            status,
                            data,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| RdpError::Runtime)?;
        Ok(Self {
            requests,
            completions,
            stopped,
        })
    }
    pub fn request(&self, request: DriveRequest) -> Result<()> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RdpError::Limit("drive request queue"),
                mpsc::error::TrySendError::Closed(_) => RdpError::FileUnavailable,
            })
    }
}
impl Drop for SharedDrives {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

struct Root {
    file: File,
    name: String,
    read_only: bool,
}
struct Handle {
    file: File,
    path: String,
    device: u32,
    delete: bool,
    write: bool,
    desired: u32,
    sharing: u32,
    enumeration: Option<(ReadDir, String)>,
}
struct Worker {
    roots: Vec<Root>,
    handles: HashMap<u32, Handle>,
    next_id: u32,
    generation: u64,
}
impl Worker {
    fn process(&mut self, request: &DriveRequest) -> IoResult<Vec<u8>> {
        if self.generation != request.generation {
            self.handles.clear();
            self.generation = request.generation;
        }
        let root = self
            .roots
            .get(request.device_id.checked_sub(1).ok_or(INVALID)? as usize)
            .ok_or(INVALID)?;
        let mut r = Reader::new(&request.data);
        if request.major == 0 {
            return self.create(request, &mut r);
        }
        let handle = self
            .handles
            .get_mut(&request.file_id)
            .filter(|h| h.device == request.device_id)
            .ok_or(0xc0000008u32)?;
        match request.major {
            2 => {
                let handle = self.handles.remove(&request.file_id).ok_or(INVALID)?;
                if handle.delete {
                    let (parent, name) = parent(&root.file, &handle.path)?;
                    let metadata = handle.file.metadata().map_err(status)?;
                    let current =
                        fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(os_status)?;
                    if current.st_ino == metadata.ino() && current.st_dev == metadata.dev() {
                        fs::unlinkat(
                            parent,
                            name,
                            if metadata.is_dir() {
                                AtFlags::REMOVEDIR
                            } else {
                                AtFlags::empty()
                            },
                        )
                        .map_err(os_status)?;
                    } else {
                        return Err(DENIED);
                    }
                }
                Ok(vec![0; 5])
            }
            3 => {
                let length = r.u32()? as usize;
                let offset = r.u64()?;
                r.take(20)?;
                if length > MAX_IO {
                    return Err(INVALID);
                }
                let mut data = vec![0; length];
                let n = handle.file.read_at(&mut data, offset).map_err(status)?;
                data.truncate(n);
                Ok(sized(data))
            }
            4 => {
                if root.read_only || !handle.write {
                    return Err(DENIED);
                }
                let length = r.u32()? as usize;
                let offset = r.u64()?;
                r.take(20)?;
                if length > MAX_IO
                    || offset
                        .checked_add(length as u64)
                        .is_none_or(|end| end > i64::MAX as u64)
                {
                    return Err(INVALID);
                }
                let data = r.take(length)?;
                r.end()?;
                handle.file.write_all_at(data, offset).map_err(status)?;
                let mut reply = (length as u32).to_le_bytes().to_vec();
                reply.push(0);
                Ok(reply)
            }
            5 => {
                let class = r.u32()?;
                let meta = handle.file.metadata().map_err(status)?;
                let data = match class {
                    4 => basic_info(&meta),
                    5 => standard_info(&meta, handle.delete),
                    35 => {
                        let mut b = attributes(&meta).to_le_bytes().to_vec();
                        b.extend_from_slice(&0u32.to_le_bytes());
                        b
                    }
                    _ => return Err(NOT_SUPPORTED),
                };
                Ok(sized(data))
            }
            6 => {
                if root.read_only {
                    return Err(DENIED);
                }
                let class = r.u32()?;
                let length = r.u32()? as usize;
                r.take(24)?;
                let mut data = Reader::new(r.take(length)?);
                r.end()?;
                match class {
                    4 => {
                        let _created = data.u64()?;
                        let accessed = data.u64()?;
                        let modified = data.u64()?;
                        let _changed = data.u64()?;
                        let attrs = data.u32()?;
                        let times = fs::Timestamps {
                            last_access: unix_time(accessed)?,
                            last_modification: unix_time(modified)?,
                        };
                        fs::futimens(&handle.file, &times).map_err(os_status)?;
                        if attrs != 0 {
                            let meta = handle.file.metadata().map_err(status)?;
                            let mut mode = meta.mode() & 0o777;
                            if attrs & 1 != 0 {
                                mode &= !0o222;
                            } else {
                                mode |= 0o200;
                            }
                            fs::fchmod(&handle.file, Mode::from_bits_truncate(mode))
                                .map_err(os_status)?;
                        }
                    }
                    13 => {
                        if handle.desired & 4 == 0 {
                            return Err(DENIED);
                        }
                        handle.delete = data.u8()? != 0;
                        if handle.delete && handle.path == "." {
                            return Err(DENIED);
                        }
                    }
                    19 | 20 => {
                        if !handle.write {
                            return Err(DENIED);
                        }
                        let size = data.u64()?;
                        if size > i64::MAX as u64 {
                            return Err(INVALID);
                        }
                        if class == 20 || size < handle.file.metadata().map_err(status)?.len() {
                            handle.file.set_len(size).map_err(status)?;
                        }
                    }
                    10 => {
                        if handle.desired & 4 == 0 {
                            return Err(DENIED);
                        }
                        let replace = data.u8()? != 0;
                        if data.u8()? != 0 || handle.path == "." {
                            return Err(DENIED);
                        }
                        let size = data.u32()? as usize;
                        let path = remote_path(data.take(size)?)?;
                        let (source_parent, source_name) = parent(&root.file, &handle.path)?;
                        let (target_parent, target_name) = parent(&root.file, &path)?;
                        let existing =
                            fs::statat(&source_parent, source_name, AtFlags::SYMLINK_NOFOLLOW)
                                .map_err(os_status)?;
                        let metadata = handle.file.metadata().map_err(status)?;
                        if existing.st_ino != metadata.ino() || existing.st_dev != metadata.dev() {
                            return Err(DENIED);
                        }
                        fs::renameat_with(
                            source_parent,
                            source_name,
                            target_parent,
                            target_name,
                            if replace {
                                RenameFlags::empty()
                            } else {
                                RenameFlags::NOREPLACE
                            },
                        )
                        .map_err(os_status)?;
                        handle.path = path;
                    }
                    _ => return Err(NOT_SUPPORTED),
                }
                Ok((length as u32).to_le_bytes().to_vec())
            }
            9 => {
                handle.file.sync_all().map_err(status)?;
                Ok(vec![0; 4])
            }
            12 if request.minor == 1 => {
                let class = r.u32()?;
                let initial = r.u8()? != 0;
                let length = r.u32()? as usize;
                r.take(23)?;
                if initial {
                    let pattern = remote_pattern(r.take(length)?)?;
                    let directory =
                        std::fs::read_dir(format!("/proc/self/fd/{}", handle.file.as_raw_fd()))
                            .map_err(status)?;
                    handle.enumeration = Some((directory, pattern));
                }
                let Some((directory, pattern)) = &mut handle.enumeration else {
                    return Err(INVALID);
                };
                for entry in directory {
                    let entry = entry.map_err(status)?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !matches_name(pattern, &name) {
                        continue;
                    }
                    let meta = std::fs::symlink_metadata(entry.path()).map_err(status)?;
                    if !meta.is_file() && !meta.is_dir() {
                        continue;
                    }
                    return Ok(sized(directory_info(class, &name, &meta)?));
                }
                Err(if initial { NOT_FOUND } else { NO_MORE })
            }
            12 | 14 | 17 => Err(NOT_SUPPORTED),
            10 => {
                let class = r.u32()?;
                let stat = fs::fstatvfs(&root.file).map_err(os_status)?;
                let mut out = Vec::new();
                match class {
                    1 => {
                        out.extend_from_slice(&0u64.to_le_bytes());
                        out.extend_from_slice(&1u32.to_le_bytes());
                        let name = utf16(&root.name);
                        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                        out.push(0);
                        out.extend(name);
                    }
                    3 | 7 => {
                        out.extend_from_slice(&stat.f_blocks.to_le_bytes());
                        out.extend_from_slice(&stat.f_bavail.to_le_bytes());
                        if class == 7 {
                            out.extend_from_slice(&stat.f_bfree.to_le_bytes());
                        }
                        out.extend_from_slice(&1u32.to_le_bytes());
                        out.extend_from_slice(&(stat.f_frsize as u32).to_le_bytes());
                    }
                    4 => {
                        out.extend_from_slice(&7u32.to_le_bytes());
                        out.extend_from_slice(&0x20u32.to_le_bytes());
                    }
                    5 => {
                        out.extend_from_slice(
                            &(0x00000003u32 | if root.read_only { 0x00080000 } else { 0 })
                                .to_le_bytes(),
                        );
                        out.extend_from_slice(&255u32.to_le_bytes());
                        let name = utf16("RustTerm");
                        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                        out.extend(name);
                    }
                    _ => return Err(NOT_SUPPORTED),
                }
                Ok(sized(out))
            }
            _ => Err(NOT_SUPPORTED),
        }
    }
    fn create(&mut self, request: &DriveRequest, r: &mut Reader<'_>) -> IoResult<Vec<u8>> {
        if self.handles.len() >= 256 {
            return Err(0xc000009au32);
        }
        let root = self
            .roots
            .get(request.device_id as usize - 1)
            .ok_or(INVALID)?;
        let desired = r.u32()?;
        r.u64()?;
        let _attrs = r.u32()?;
        let sharing = r.u32()?;
        let disposition = r.u32()?;
        let options = r.u32()?;
        let length = r.u32()? as usize;
        let path = remote_path(r.take(length)?)?;
        r.end()?;
        if disposition > 5 || sharing & !7 != 0 {
            return Err(INVALID);
        }
        let access = u32::from(desired & (0x80000000 | 0x10000000 | 1 | 8 | 0x80) != 0)
            | (u32::from(desired & (0x40000000 | 0x10000000 | 2 | 4 | 0x10 | 0x100) != 0) << 1)
            | (u32::from(desired & (0x10000 | 0x10000000) != 0) << 2);
        let write = access & 2 != 0;
        let delete = options & 0x1000 != 0;
        if delete && access & 4 == 0 {
            return Err(DENIED);
        }
        if root.read_only && (access & 6 != 0 || disposition != 1 || delete) {
            return Err(DENIED);
        }
        let directory = options & 1 != 0;
        let flags = (if directory {
            OFlags::RDONLY | OFlags::DIRECTORY
        } else if write {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        }) | OFlags::CLOEXEC
            | OFlags::NONBLOCK;
        let existed = match open(&root.file, &path, OFlags::PATH) {
            Ok(file) => {
                let meta = file.metadata().map_err(status)?;
                if !meta.is_file() && !meta.is_dir() {
                    return Err(DENIED);
                }
                true
            }
            Err(NOT_FOUND | 0xc0000034 | 0xc000003a) => false,
            Err(error) => return Err(error),
        };
        if directory && matches!(disposition, 0 | 2 | 3 | 5) && !existed {
            let (parent, name) = parent(&root.file, &path)?;
            fs::mkdirat(parent, name, Mode::from_bits_truncate(0o700)).map_err(os_status)?;
        } else if directory && disposition == 2 && existed {
            return Err(0xc0000035u32);
        }
        let create = if directory {
            OFlags::empty()
        } else {
            match disposition {
                0 | 3 | 5 => OFlags::CREATE,
                2 => OFlags::CREATE | OFlags::EXCL,
                _ => OFlags::empty(),
            }
        };
        if !write && matches!(disposition, 0 | 4 | 5) {
            return Err(DENIED);
        }
        let file = open(&root.file, &path, flags | create)?;
        let meta = file.metadata().map_err(status)?;
        if !meta.is_dir() && !meta.is_file() {
            return Err(DENIED);
        }
        if options & 0x40 != 0 && meta.is_dir() {
            return Err(0xc00000bau32);
        }
        if delete && path == "." {
            return Err(DENIED);
        }
        for handle in self
            .handles
            .values()
            .filter(|h| h.device == request.device_id)
        {
            let other = handle.file.metadata().map_err(status)?;
            if other.ino() == meta.ino()
                && other.dev() == meta.dev()
                && (access & !handle.sharing != 0 || handle.desired & !sharing != 0)
            {
                return Err(0xc0000043u32);
            }
        }
        if !directory && matches!(disposition, 0 | 4 | 5) {
            file.set_len(0).map_err(status)?;
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(INVALID)?;
        self.handles.insert(
            id,
            Handle {
                file,
                path,
                device: request.device_id,
                delete,
                write,
                desired: access,
                sharing,
                enumeration: None,
            },
        );
        let mut result = id.to_le_bytes().to_vec();
        result.push(if !existed {
            2
        } else if disposition == 0 {
            0
        } else if matches!(disposition, 4 | 5) {
            3
        } else {
            1
        });
        Ok(result)
    }
}
fn open(root: &File, path: &str, flags: OFlags) -> IoResult<File> {
    fs::openat2(
        root,
        path,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        if flags.contains(OFlags::CREATE) {
            Mode::from_bits_truncate(0o600)
        } else {
            Mode::empty()
        },
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map(File::from)
    .map_err(os_status)
}
fn parent<'a>(root: &File, path: &'a str) -> IoResult<(File, &'a str)> {
    if path == "." {
        return Err(DENIED);
    }
    let (directory, name) = path.rsplit_once('/').unwrap_or((".", path));
    Ok((
        open(root, directory, OFlags::RDONLY | OFlags::DIRECTORY)?,
        name,
    ))
}
fn remote_path(bytes: &[u8]) -> IoResult<String> {
    let text = decode_path(bytes)?;
    let path = text.trim_start_matches(['\\', '/']);
    if path.is_empty() {
        return Ok(".".into());
    }
    if path.len() > 4096 {
        return Err(INVALID);
    }
    let parts: Vec<_> = path.split(['\\', '/']).collect();
    if parts.iter().any(|p| {
        p.is_empty()
            || *p == "."
            || *p == ".."
            || p.contains(':')
            || p.chars().any(char::is_control)
    }) {
        return Err(DENIED);
    }
    Ok(parts.join("/"))
}
fn remote_pattern(bytes: &[u8]) -> IoResult<String> {
    let text = decode_path(bytes)?;
    let pattern = text.rsplit(['\\', '/']).next().unwrap_or("*");
    if pattern.len() > 255 {
        return Err(INVALID);
    }
    Ok(if pattern == "*.*" || pattern.is_empty() {
        "*".into()
    } else {
        pattern.to_lowercase()
    })
}
fn decode_path(bytes: &[u8]) -> IoResult<String> {
    if !bytes.len().is_multiple_of(2) || bytes.len() > 8194 {
        return Err(INVALID);
    }
    let mut units: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    if units.last() == Some(&0) {
        units.pop();
    }
    if units.contains(&0) {
        return Err(INVALID);
    }
    String::from_utf16(&units).map_err(|_| INVALID)
}
fn matches_name(pattern: &str, name: &str) -> bool {
    let pattern: Vec<_> = pattern.chars().collect();
    let name: Vec<_> = name.to_lowercase().chars().collect();
    let (mut p, mut n, mut star, mut checkpoint) = (0, 0, None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            checkpoint = n;
        } else if let Some(s) = star {
            checkpoint += 1;
            n = checkpoint;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
fn attributes(meta: &Metadata) -> u32 {
    (if meta.is_dir() { 0x10 } else { 0x20 }) | u32::from(meta.mode() & 0o200 == 0)
}
fn filetime(seconds: i64, nanos: i64) -> u64 {
    (i128::from(seconds) + 11_644_473_600)
        .saturating_mul(10_000_000)
        .saturating_add(i128::from(nanos) / 100)
        .clamp(0, i128::from(u64::MAX)) as u64
}
fn unix_time(value: u64) -> IoResult<rustix::fs::Timespec> {
    if value == 0 || value == u64::MAX {
        return Ok(rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: fs::UTIME_OMIT,
        });
    }
    let seconds = (value / 10_000_000) as i64 - 11_644_473_600;
    Ok(rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: ((value % 10_000_000) * 100) as _,
    })
}
fn basic_info(meta: &Metadata) -> Vec<u8> {
    let mut out = Vec::new();
    for time in [
        filetime(meta.ctime(), meta.ctime_nsec()),
        filetime(meta.atime(), meta.atime_nsec()),
        filetime(meta.mtime(), meta.mtime_nsec()),
        filetime(meta.ctime(), meta.ctime_nsec()),
    ] {
        out.extend_from_slice(&time.to_le_bytes());
    }
    out.extend_from_slice(&attributes(meta).to_le_bytes());
    out
}
fn standard_info(meta: &Metadata, delete: bool) -> Vec<u8> {
    let mut out = (meta.blocks() * 512).to_le_bytes().to_vec();
    out.extend_from_slice(&meta.len().to_le_bytes());
    out.extend_from_slice(&(meta.nlink() as u32).to_le_bytes());
    out.extend_from_slice(&[u8::from(delete), u8::from(meta.is_dir())]);
    out
}
fn directory_info(class: u32, name: &str, meta: &Metadata) -> IoResult<Vec<u8>> {
    let name = utf16(name);
    let mut out = vec![0; 8];
    if class == 12 {
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend(name);
        return Ok(out);
    }
    if !matches!(class, 1..=3) {
        return Err(NOT_SUPPORTED);
    }
    let basic = basic_info(meta);
    out.extend_from_slice(&basic[..32]);
    out.extend_from_slice(&meta.len().to_le_bytes());
    out.extend_from_slice(&(meta.blocks() * 512).to_le_bytes());
    out.extend_from_slice(&basic[32..]);
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    if class >= 2 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    if class == 3 {
        out.extend_from_slice(&[0; 25]);
    }
    out.extend(name);
    Ok(out)
}
fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}
fn sized(data: Vec<u8>) -> Vec<u8> {
    let mut out = (data.len() as u32).to_le_bytes().to_vec();
    out.extend(data);
    out
}
fn error_body(major: u32) -> Vec<u8> {
    vec![0; if matches!(major, 0 | 2 | 4) { 5 } else { 4 }]
}
fn os_status(error: rustix::io::Errno) -> u32 {
    status(io::Error::from_raw_os_error(error.raw_os_error()))
}
fn status(error: io::Error) -> u32 {
    match error.raw_os_error() {
        Some(libc::ENOENT) => NOT_FOUND,
        Some(libc::EEXIST) => 0xc0000035,
        Some(libc::ENOTEMPTY) => 0xc0000101,
        Some(libc::ENOSPC) => 0xc000007f,
        Some(libc::ENOTDIR) => 0xc0000103,
        Some(libc::EISDIR) => 0xc00000ba,
        Some(libc::EBADF) => 0xc0000008,
        _ => DENIED,
    }
}
struct Reader<'a> {
    data: &'a [u8],
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
    fn take(&mut self, n: usize) -> IoResult<&'a [u8]> {
        if n > self.data.len() {
            return Err(INVALID);
        }
        let (a, b) = self.data.split_at(n);
        self.data = b;
        Ok(a)
    }
    fn u8(&mut self) -> IoResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> IoResult<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| INVALID)?,
        ))
    }
    fn u64(&mut self) -> IoResult<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| INVALID)?,
        ))
    }
    fn end(&self) -> IoResult<()> {
        if self.data.is_empty() {
            Ok(())
        } else {
            Err(INVALID)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_protocol_accepted_requests_fit_including_closes_in_order() {
        let (requests, mut received) = mpsc::channel(MAX_PENDING_REQUESTS);
        let (_completed, completions) = mpsc::channel(64);
        let drives = SharedDrives {
            requests,
            completions,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let request = |id: u32| DriveRequest {
            generation: 1,
            device_id: 1,
            file_id: id / 4,
            completion_id: id,
            major: [0, 3, 4, 2][id as usize % 4],
            minor: 0,
            data: Vec::new(),
        };
        for id in 0..256 {
            drives.request(request(id)).unwrap();
        }
        assert_eq!(
            drives.request(request(256)),
            Err(RdpError::Limit("drive request queue"))
        );
        for id in 0..256 {
            assert_eq!(received.try_recv().unwrap(), request(id));
        }
        drives.request(request(256)).unwrap();
        assert_eq!(received.try_recv().unwrap(), request(256));
        drop(received);
        assert_eq!(drives.request(request(257)), Err(RdpError::FileUnavailable));
    }

    #[test]
    fn paths_and_symlinks_cannot_escape_an_explicit_share() {
        assert_eq!(
            remote_path(&utf16("\\folder\\file.txt")).unwrap(),
            "folder/file.txt"
        );
        for path in [
            "\\..\\outside",
            "C:\\secret",
            "\\folder\\..\\secret",
            "\\file:stream",
            "\\a\\\\b",
        ] {
            assert!(remote_path(&utf16(path)).is_err());
        }
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"not shared").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let fd = File::open(root.path()).unwrap();
        assert!(open(&fd, "link/secret", OFlags::RDONLY).is_err());
        assert!(open(&fd, "../secret", OFlags::RDONLY).is_err());
    }
    #[test]
    fn enumeration_handles_windows_wildcards_without_regex_allocation() {
        assert!(matches_name("*.txt", "Report.TXT"));
        assert!(matches_name("file?.*", "file1.json"));
        assert!(!matches_name("*.txt", "txt.bin"));
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    fn request(major: u32, file_id: u32, data: Vec<u8>) -> DriveRequest {
        DriveRequest {
            generation: 1,
            device_id: 1,
            completion_id: 1,
            major,
            minor: 0,
            file_id,
            data,
        }
    }
    fn create(path: &str, disposition: u32, write: bool) -> DriveRequest {
        let mut data = (if write { 0xc0010000u32 } else { 0x80000000 })
            .to_le_bytes()
            .to_vec();
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&7u32.to_le_bytes());
        data.extend_from_slice(&disposition.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        let path = utf16(path);
        data.extend_from_slice(&(path.len() as u32).to_le_bytes());
        data.extend(path);
        request(0, 0, data)
    }
    fn worker(path: &std::path::Path, read_only: bool) -> Worker {
        Worker {
            roots: vec![Root {
                file: File::open(path).unwrap(),
                name: "Local1".into(),
                read_only,
            }],
            handles: HashMap::new(),
            next_id: 1,
            generation: 0,
        }
    }
    #[test]
    fn writable_share_supports_create_write_read_and_read_only_rejects_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let mut w = worker(directory.path(), false);
        let opened = w.process(&create("\\hello.txt", 2, true)).unwrap();
        let id = u32::from_le_bytes(opened[..4].try_into().unwrap());
        let mut data = 3u32.to_le_bytes().to_vec();
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&[0; 20]);
        data.extend_from_slice(b"abc");
        assert_eq!(w.process(&request(4, id, data)).unwrap(), [3, 0, 0, 0, 0]);
        let mut read = 3u32.to_le_bytes().to_vec();
        read.extend_from_slice(&0u64.to_le_bytes());
        read.extend_from_slice(&[0; 20]);
        assert_eq!(
            w.process(&request(3, id, read)).unwrap(),
            [3, 0, 0, 0, b'a', b'b', b'c']
        );
        let mut ro = worker(directory.path(), true);
        assert_eq!(ro.process(&create("\\new.txt", 2, true)), Err(DENIED));
        assert!(ro.process(&create("\\hello.txt", 1, false)).is_ok());
    }
    #[test]
    fn query_volume_and_file_metadata_use_correct_irp_codes() {
        let directory = tempfile::tempdir().unwrap();
        let mut w = worker(directory.path(), false);
        let opened = w.process(&create("", 1, false)).unwrap();
        let id = u32::from_le_bytes(opened[..4].try_into().unwrap());
        let reply = w
            .process(&request(10, id, 3u32.to_le_bytes().to_vec()))
            .unwrap();
        assert_eq!(reply.len(), 28);
        let reply = w
            .process(&request(5, id, 5u32.to_le_bytes().to_vec()))
            .unwrap();
        assert_eq!(reply.len(), 26);
        assert_eq!(reply[25], 1);
    }
}
