use super::*;
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

const MANIFEST: &str = "share/doc/rustterm/build-info.json";
const PACKAGE_FILES: &[&str] = &[
    "bin/rustterm",
    "build-info.json",
    "share/applications/io.rustterm.RustTerm.desktop",
    "share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg",
    "share/doc/rustterm/LICENSE",
    "share/doc/rustterm/README.md",
    "share/doc/rustterm/alacritty_terminal-LICENSE-APACHE",
];
const INSTALLER: &str = include_str!("installer/install.py");

#[derive(Clone, Debug)]
pub struct Installation {
    prefix: PathBuf,
    manifest_sha256: [u8; 32],
}
impl Installation {
    pub fn current() -> Result<Self> {
        Self::at_executable(&std::env::current_exe()?)
    }

    pub fn at_executable(executable: &Path) -> Result<Self> {
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")),
            "Unsupported update platform"
        );
        ensure!(
            executable.is_absolute()
                && executable
                    .file_name()
                    .is_some_and(|name| name == "rustterm"),
            "Not an installed RustTerm executable"
        );
        let bin = executable.parent().context("No executable directory")?;
        ensure!(
            bin.file_name().is_some_and(|name| name == "bin"),
            "Install with install.sh to enable in-app updates"
        );
        let prefix = bin
            .parent()
            .context("No installation prefix")?
            .to_path_buf();
        let marker = read_bounded(File::open(prefix.join(MANIFEST))?, 16384)?;
        let manifest: serde_json::Value = serde_json::from_slice(&marker)?;
        if let Some(version) = manifest["version"].as_str() {
            ensure!(
                version == CURRENT_VERSION,
                "The installed version changed. Restart RustTerm before updating again."
            );
        }
        let installation = Self {
            prefix,
            manifest_sha256: Sha256::digest(&marker).into(),
        };
        installation.validate()?;
        Ok(installation)
    }

    pub fn executable(&self) -> PathBuf {
        self.prefix.join("bin/rustterm")
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.prefix != Path::new("/"), "Invalid installation prefix");
        let marker = self.prefix.join(MANIFEST);
        ensure!(
            fs::symlink_metadata(&marker)?.file_type().is_file(),
            "Invalid installation manifest"
        );
        let marker = read_bounded(File::open(marker)?, 16384)?;
        ensure!(
            <[u8; 32]>::from(Sha256::digest(&marker)) == self.manifest_sha256,
            "The installed version changed. Restart RustTerm before updating again."
        );
        let manifest: serde_json::Value = serde_json::from_slice(&marker)?;
        ensure!(
            manifest["installation"] != "deb",
            "Update Debian packages by installing the new .deb with APT"
        );
        ensure!(
            manifest["platform"] == "linux-x86_64" && manifest["variant"] == "release",
            "Install a release package with install.sh to enable in-app updates"
        );
        ensure!(
            fs::symlink_metadata(self.executable())?
                .file_type()
                .is_file(),
            "Installed executable is not a regular file"
        );
        // The installer also validates every destination, owner, and directory
        // before its transaction. Check the prerequisites before downloading.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(&self.prefix)?;
            ensure!(
                metadata.uid() == rustix::process::geteuid().as_raw()
                    && metadata.mode() & 0o022 == 0,
                "Installation directory must be owned by this user and not writable by others"
            );
        }
        Ok(())
    }

    pub(super) fn install(&self, package: &Path) -> Result<()> {
        self.validate()?;
        // Execute our bundled installer, never scripts from a downloaded archive.
        fs::write(package.join("install.py"), INSTALLER)?;
        let status = Command::new("python3")
            .arg("-I")
            .arg(package.join("install.py"))
            .arg("--prefix")
            .arg(&self.prefix)
            .arg("--expected-manifest-sha256")
            .arg(
                self.manifest_sha256
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            )
            .stdin(Stdio::null())
            .output()
            .context("Could not run the installer; Python 3.11 or newer is required")?;
        ensure!(
            status.status.success(),
            "Installation failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
        Ok(())
    }
}

pub(super) fn unpack(archive: &Path, staging: &Path, release: &Release) -> Result<PathBuf> {
    let root = release
        .archive
        .name
        .strip_suffix(".tar.gz")
        .context("Invalid archive name")?;
    let package = staging.join("package");
    fs::create_dir(&package)?;
    let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
    // Bound the *decompressed* stream as well as individual files (tar padding
    // and extension headers must not provide a decompression-bomb loophole).
    let mut archive = tar::Archive::new(decoder.take(1024 * 1024 * 1024 + 1));
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for (index, entry) in archive.entries()?.raw(true).enumerate() {
        ensure!(index < 64, "Too many archive entries");
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(
            path.components()
                .all(|part| matches!(part, Component::Normal(_))),
            "Unsafe archive path"
        );
        let relative = path.strip_prefix(root).context("Unexpected archive root")?;
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "Archive links and special files are not allowed"
        );
        if kind.is_dir() {
            ensure!(entry.size() == 0, "Invalid archive directory");
            continue;
        }
        let relative = relative.to_str().context("Invalid archive filename")?;
        ensure!(
            PACKAGE_FILES.contains(&relative) || matches!(relative, "install.py" | "install.sh"),
            "Unexpected archive file: {relative}"
        );
        ensure!(seen.insert(relative.to_owned()), "Duplicate archive file");
        total = total
            .checked_add(entry.size())
            .context("Archive size overflow")?;
        ensure!(
            total <= 1024 * 1024 * 1024 && entry.size() <= MAX_ARCHIVE,
            "Unpacked archive exceeds size limit"
        );
        if !PACKAGE_FILES.contains(&relative) {
            continue;
        }
        let target = package.join(relative);
        fs::create_dir_all(target.parent().context("Invalid archive path")?)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let copied = std::io::copy(&mut entry, &mut output)?;
        ensure!(copied == entry.size(), "Truncated archive entry");
    }
    // Consume padding and the gzip trailer too, validating the decompressor's
    // checksum and enforcing the bound even after tar's end-of-archive marker.
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    ensure!(decoder.limit() > 0, "Unpacked archive exceeds size limit");
    ensure!(
        PACKAGE_FILES.iter().all(|name| seen.contains(*name)),
        "Release archive is missing required files"
    );
    let manifest: serde_json::Value = serde_json::from_slice(&read_bounded(
        File::open(package.join("build-info.json"))?,
        16384,
    )?)?;
    ensure!(
        manifest["platform"] == "linux-x86_64"
            && manifest["variant"] == "release"
            && manifest["version"] == release.version,
        "Release manifest does not match the selected version/platform"
    );
    let expected = parse_hash(
        manifest["binary_sha256"]
            .as_str()
            .context("Missing binary checksum")?,
    )?;
    let mut hash = Sha256::new();
    std::io::copy(&mut File::open(package.join("bin/rustterm"))?, &mut hash)?;
    ensure!(
        <[u8; 32]>::from(hash.finalize()) == expected,
        "Release executable checksum failed"
    );
    Ok(package)
}
