//! Verified Windows ZIP payloads and replacement transactions. No downloaded code
//! runs during preparation; the existing executable supplies the update helper.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};

// The packager and updater consume the same Cargo metadata contract.
include!(concat!(env!("OUT_DIR"), "/windows_release.rs"));
pub(super) const PREFIX: &str = ".rustterm-update-";

#[derive(Debug)]
pub(super) struct Prepared {
    // The transaction core is also compiled on Linux for archive/rollback tests.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) directory: tempfile::TempDir,
    _lock: File,
}
impl Prepared {
    #[cfg(windows)]
    pub(super) fn handoff(self) {
        let _ = self.directory.keep();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub(super) version: String,
    pub(super) before: BTreeMap<String, [u8; 32]>,
    pub(super) after: BTreeMap<String, [u8; 32]>,
}

pub(super) fn regular(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Update paths must be regular files and directories"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Update paths must not be reparse points"
        );
    }
    Ok(())
}

pub(super) fn hash(path: &Path) -> Result<[u8; 32]> {
    regular(path, false)?;
    ensure!(
        fs::metadata(path)?.len() <= MAX_ARCHIVE,
        "Update file is too large"
    );
    let mut hash = Sha256::new();
    std::io::copy(&mut File::open(path)?.take(MAX_ARCHIVE + 1), &mut hash)?;
    Ok(hash.finalize().into())
}

pub(super) fn validate_installation(prefix: &Path, version: &str) -> Result<[u8; 32]> {
    regular(prefix, true)?;
    let marker = prefix.join(MANIFEST);
    regular(&marker, false)?;
    let bytes = read_bounded(File::open(&marker)?, 16384)?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest["platform"] == "windows-x86_64"
            && manifest["variant"] == "release"
            && manifest["version"] == version,
        "Extract an official Windows release ZIP to enable in-app updates"
    );
    let expected = parse_hash(
        manifest["binary_sha256"]
            .as_str()
            .context("Missing binary checksum")?,
    )?;
    ensure!(
        hash(&prefix.join(BINARY))? == expected,
        "Portable executable checksum failed"
    );
    let mut executable = File::open(prefix.join(BINARY))?;
    let mut header = [0u8; 64];
    executable.read_exact(&mut header)?;
    ensure!(&header[..2] == b"MZ", "Expected a Windows executable");
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap());
    ensure!(
        (64..=1024 * 1024).contains(&offset),
        "Invalid Windows executable header"
    );
    executable.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut signature = [0u8; 6];
    executable.read_exact(&mut signature)?;
    ensure!(
        &signature == b"PE\0\0\x64\x86",
        "Expected an x86_64 Windows executable"
    );
    for name in FILES {
        regular(&prefix.join(name), false)?;
    }
    Ok(Sha256::digest(bytes).into())
}

pub(super) fn unpack(archive: &Path, staging: &Path, release: &Release) -> Result<PathBuf> {
    let root = release
        .archive
        .name
        .strip_suffix(".zip")
        .context("Invalid ZIP name")?;
    let package = staging.join("package");
    fs::create_dir(&package)?;
    let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(archive.len() <= 16, "Too many ZIP entries");
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        ensure!(
            !entry.encrypted(),
            "Encrypted update ZIPs are not supported"
        );
        let name = std::str::from_utf8(entry.name_raw())
            .context("Invalid ZIP filename")?
            .to_owned();
        if entry.is_dir() {
            ensure!(
                name == format!("{root}/") && entry.size() == 0,
                "Unexpected ZIP directory"
            );
            continue;
        }
        let relative = name
            .strip_prefix(&format!("{root}/"))
            .context("Unexpected ZIP root")?;
        // Exact ASCII names also reject traversal, drive names, ADS, backslashes,
        // trailing dots/spaces, and case aliases on Windows.
        ensure!(FILES.contains(&relative), "Unexpected ZIP file");
        ensure!(seen.insert(relative.to_owned()), "Duplicate ZIP file");
        ensure!(
            entry
                .unix_mode()
                .is_none_or(|mode| matches!(mode & 0o170000, 0 | 0o100000)),
            "ZIP links and special files are not allowed"
        );
        let size = entry.size();
        total = total.checked_add(size).context("ZIP size overflow")?;
        ensure!(
            size <= MAX_ARCHIVE && total <= 1024 * 1024 * 1024,
            "Unpacked ZIP exceeds size limit"
        );
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(package.join(relative))?;
        let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut output)?;
        ensure!(copied == size, "Invalid ZIP entry size");
        output.sync_all()?;
    }
    ensure!(
        FILES.iter().all(|name| seen.contains(*name)),
        "ZIP is missing required files"
    );
    validate_installation(&package, &release.version)?;
    Ok(package)
}

pub(super) fn lock(prefix: &Path) -> Result<File> {
    let path = prefix.join(".rustterm-update.lock");
    if path.try_exists()? {
        regular(&path, false)?;
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .context("The portable folder must be writable to install updates")?;
    lock.try_lock()
        .context("Another update is using this portable folder")?;
    Ok(lock)
}

fn copy_synced(source: &Path, destination: &Path) -> Result<()> {
    regular(source, false)?;
    let mut input = File::open(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

pub(super) fn prepare(
    prefix: &Path,
    package: &Path,
    version: &str,
    expected: [u8; 32],
) -> Result<Prepared> {
    let lock = lock(prefix)?;
    ensure!(
        validate_installation(prefix, CURRENT_VERSION)? == expected,
        "The installed version changed before updating"
    );
    validate_installation(package, version)?;
    ensure!(
        Version::parse(version)?
            .cmp_precedence(&Version::parse(CURRENT_VERSION)?)
            .is_gt(),
        "Updates must be newer than the installed version"
    );
    let directory = tempfile::Builder::new().prefix(PREFIX).tempdir_in(prefix)?;
    let new = directory.path().join("new");
    let backup = directory.path().join("backup");
    fs::create_dir(&new)?;
    fs::create_dir(&backup)?;
    let mut plan = Plan {
        version: version.into(),
        before: BTreeMap::new(),
        after: BTreeMap::new(),
    };
    for name in FILES {
        copy_synced(&prefix.join(name), &backup.join(name))?;
        copy_synced(&package.join(name), &new.join(name))?;
        plan.before
            .insert((*name).into(), hash(&backup.join(name))?);
        plan.after.insert((*name).into(), hash(&new.join(name))?);
    }
    ensure!(
        plan.before[MANIFEST] == expected,
        "The installed version changed while preparing the update"
    );
    copy_synced(&backup.join(BINARY), &directory.path().join("helper.exe"))?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.path().join("plan.json"))?;
    serde_json::to_writer(&mut output, &plan)?;
    output.sync_all()?;
    Ok(Prepared {
        directory,
        _lock: lock,
    })
}

#[cfg(any(windows, test))]
pub(super) fn plan(directory: &Path) -> Result<Plan> {
    regular(directory, true)?;
    ensure!(
        directory
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with(PREFIX)),
        "Invalid update directory"
    );
    regular(&directory.join("plan.json"), false)?;
    let plan: Plan = serde_json::from_slice(&read_bounded(
        File::open(directory.join("plan.json"))?,
        16384,
    )?)?;
    ensure!(
        plan.before.len() == FILES.len()
            && plan.after.len() == FILES.len()
            && FILES
                .iter()
                .all(|name| plan.before.contains_key(*name) && plan.after.contains_key(*name)),
        "Invalid update file list"
    );
    Ok(plan)
}

#[cfg(any(windows, test))]
fn rollback(directory: &Path, plan: &Plan) -> Result<()> {
    let prefix = directory.parent().context("No installation directory")?;
    // Validate all backups before replacing any destination.
    regular(&directory.join("backup"), true)?;
    for name in FILES {
        ensure!(
            hash(&directory.join("backup").join(name))? == plan.before[*name],
            "Update backup checksum failed"
        );
    }
    // Rollback can also follow a committed replacement if the new process fails
    // to start. Keep recovery enabled if rollback itself is interrupted.
    File::create(directory.join("applying"))?.sync_all()?;
    if directory.join("complete").try_exists()? {
        fs::remove_file(directory.join("complete"))?;
    }
    for name in FILES.iter().rev() {
        let destination = prefix.join(name);
        let current = hash(&destination)?;
        if current == plan.before[*name] {
            continue;
        }
        ensure!(
            current == plan.after[*name],
            "An installation file changed during rollback; backups were preserved"
        );
        let temporary = directory.join("restore");
        if temporary.try_exists()? {
            regular(&temporary, false)?;
            fs::remove_file(&temporary)?;
        }
        copy_synced(&directory.join("backup").join(name), &temporary)?;
        fs::rename(&temporary, destination)?;
    }
    if directory.join("applying").try_exists()? {
        fs::remove_file(directory.join("applying"))?;
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(super) fn apply(directory: &Path) -> Result<()> {
    apply_with(directory, |_| Ok(()))
}

#[cfg(windows)]
pub(super) fn undo(directory: &Path) -> Result<()> {
    rollback(directory, &plan(directory)?)
}

#[cfg(any(windows, test))]
fn apply_with(directory: &Path, mut before_replace: impl FnMut(usize) -> Result<()>) -> Result<()> {
    let plan = plan(directory)?;
    let prefix = directory.parent().context("No installation directory")?;
    if directory.join("applying").try_exists()? {
        rollback(directory, &plan)?;
    }
    regular(&directory.join("new"), true)?;
    for name in FILES {
        ensure!(
            hash(&prefix.join(name))? == plan.before[*name],
            "The installed files changed before replacement"
        );
        ensure!(
            hash(&directory.join("new").join(name))? == plan.after[*name],
            "Staged update checksum failed"
        );
    }
    validate_installation(&directory.join("new"), &plan.version)?;
    File::create(directory.join("applying"))?.sync_all()?;
    let replace = (|| -> Result<()> {
        for (index, name) in FILES.iter().enumerate() {
            before_replace(index)?;
            // Copy instead of consuming the staged payload so an interrupted
            // transaction can be rolled back and retried.
            let temporary = directory.join("replacement");
            if temporary.try_exists()? {
                regular(&temporary, false)?;
                fs::remove_file(&temporary)?;
            }
            copy_synced(&directory.join("new").join(name), &temporary)?;
            fs::rename(&temporary, prefix.join(name))?;
        }
        validate_installation(prefix, &plan.version)?;
        File::create(directory.join("complete"))?.sync_all()?;
        fs::remove_file(directory.join("applying"))?;
        Ok(())
    })();
    if let Err(error) = replace {
        rollback(directory, &plan).context(format!("Update failed ({error}); rollback could not finish. Backups are preserved in the update directory"))?;
        return Err(error).context("Update failed; the previous version was restored");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
