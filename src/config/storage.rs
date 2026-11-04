use crate::i18n::messages as i18n_key;

use super::{ConfigError, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Keep this guard alive for the application lifetime. OS locks are released on crashes.
pub struct ConfigLock {
    _file: fs::File,
}

impl ConfigLock {
    pub fn acquire(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        ensure_directory(directory)?;
        let path = directory.join("instance.lock");
        check_existing_file(&path)?;
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let file = options.open(&path)?;
        check_file(&path, &file.metadata()?)?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => ConfigError::AlreadyRunning,
            fs::TryLockError::Error(error) => ConfigError::Io(error),
        })?;
        Ok(Self { _file: file })
    }
}

pub(super) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > MAX_FILE_BYTES
            {
                return Err(ConfigError::Permissions(path.to_owned()));
            }
            check_owner(path, &metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    check_owner(parent, &fs::metadata(parent)?)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // NONBLOCK also avoids hanging if a file was replaced by a FIFO after lstat.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options.open(path)?;
    check_file(path, &file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(super::invalid(
            i18n_key::CONNECTIONS_THE_CONFIGURATION_FILE_IS_TOO_LARGE,
        ));
    }
    let source = String::from_utf8(bytes).map_err(|_| {
        super::invalid(i18n_key::CONNECTIONS_THE_CONFIGURATION_FILE_IS_NOT_VALID_UTF_8)
    })?;
    Ok(Some(source))
}

pub(super) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if contents.len() as u64 > MAX_FILE_BYTES {
        return Err(super::invalid(
            i18n_key::CONNECTIONS_THE_CONFIGURATION_FILE_IS_TOO_LARGE,
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_directory(parent)?;
    check_existing_file(path)?;
    // Open before the rename so an open failure cannot be mistaken for a failed commit.
    #[cfg(unix)]
    let directory = fs::File::open(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|e| ConfigError::Io(e.error))?;
    #[cfg(unix)]
    directory.sync_all().map_err(ConfigError::CommitUncertain)?;
    Ok(())
}

fn check_existing_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_file(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn check_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_FILE_BYTES {
        return Err(ConfigError::Permissions(path.to_owned()));
    }
    check_owner(path, metadata)
}

fn ensure_directory(directory: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    check_owner(directory, &fs::metadata(directory)?)
}

fn check_owner(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(ConfigError::Permissions(path.to_owned()));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, metadata);
    Ok(())
}

pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(value).join("RustTerm"))
            .filter(|path| path.is_absolute())
            .ok_or(ConfigError::MissingConfigDirectory);
    }
    #[cfg(not(windows))]
    config_dir_unix()
}
#[cfg(not(windows))]
fn config_dir_unix() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path.join("rustterm"));
        }
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(".config/rustterm"))
        .ok_or(ConfigError::MissingConfigDirectory)
}
