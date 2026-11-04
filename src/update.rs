//! Release updates use authenticated HTTPS, bounded downloads, verified archives,
//! and the same transactional installer as manual installations.
mod install;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use anyhow::{Context, Result, bail, ensure};
pub use install::Installation;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};
use url::Url;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASES_URL: &str = "https://github.com/oksyd/RustTerm/releases";
const LATEST_URL: &str = "https://api.github.com/repos/oksyd/RustTerm/releases/latest";
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const MAX_METADATA: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    archive: Asset,
    checksum: Asset,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

/// Cancellation and installation share one atomic transition, so quitting cannot
/// race a final cancellation check and start an installer after the app exits.
#[derive(Default)]
pub struct UpdateControl(AtomicU8);
impl UpdateControl {
    pub fn cancel(&self) -> bool {
        match self
            .0
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(1 | 3) => true,
            Err(_) => false,
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst) == 1
    }
    pub fn can_cancel(&self) -> bool {
        self.0.load(Ordering::SeqCst) == 0
    }
    fn begin_install(&self) -> Result<InstallGuard<'_>> {
        ensure!(
            self.0
                .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "Update cancelled"
        );
        Ok(InstallGuard(self))
    }
}
struct InstallGuard<'a>(&'a UpdateControl);
impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        self.0.0.store(3, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Progress {
    Download { received: u64, total: u64 },
    Verifying,
    Installing,
}

/// Blank explicitly means direct connection; this setting never changes sessions.
/// Credentials are not accepted in this plain-text preference.
pub fn validate_proxy(proxy: &str) -> Result<()> {
    let invalid = || {
        anyhow::anyhow!(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_INVALID_PROXY
        ))
    };
    if proxy.is_empty() {
        return Ok(());
    }
    ensure!(
        proxy.len() <= 2048 && !proxy.chars().any(char::is_whitespace),
        invalid()
    );
    let url = Url::parse(proxy).map_err(|_| invalid())?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.port() != Some(0)
            && url.username().is_empty()
            && url.password().is_none()
            && !proxy.contains('@')
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none(),
        invalid()
    );
    ureq::Proxy::new(proxy).map_err(|_| invalid())?;
    Ok(())
}

fn client(proxy: &str, timeout: Duration) -> Result<ureq::Agent> {
    validate_proxy(proxy)?;
    let proxy = if proxy.is_empty() {
        None
    } else {
        Some(ureq::Proxy::new(proxy)?)
    };
    let native = rustls_native_certs::load_native_certs().certs;
    let roots = if native.is_empty() {
        ureq::tls::RootCerts::WebPki
    } else {
        native
            .into_iter()
            .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
            .collect::<Vec<_>>()
            .into()
    };
    Ok(ureq::Agent::config_builder()
        .user_agent(concat!("RustTerm/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .proxy(proxy)
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
        .max_redirects(5)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(timeout))
        .build()
        .into())
}

pub fn check(proxy: &str) -> Result<Option<Release>> {
    let agent = client(proxy, Duration::from_secs(30))?;
    let response = agent
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match response {
        Err(ureq::Error::StatusCode(404)) => bail!(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_NO_RELEASE
        )),
        Err(ureq::Error::StatusCode(403 | 429)) => bail!(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_RATE_LIMIT
        )),
        other => other.context("GitHub Releases request failed")?,
    };
    let bytes = read_bounded(response.body_mut().as_reader(), MAX_METADATA)?;
    select_release(
        serde_json::from_slice(&bytes).context("Invalid GitHub release response")?,
        CURRENT_VERSION,
    )
}

fn select_release(release: GithubRelease, current: &str) -> Result<Option<Release>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .context("Release tag must be a semantic version, e.g. 0.2.0")?;
    if !version.pre.is_empty() || !version.cmp_precedence(&Version::parse(current)?).is_gt() {
        return Ok(None);
    }
    let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64.tar.gz"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x86_64.zip"
    } else {
        bail!("No release artifact is available for this platform");
    };
    let name = format!("rustterm-{version}-{platform}");
    let asset = |name: &str, maximum| -> Result<Asset> {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let found = matches
            .next()
            .context(format!("Release is missing {name}"))?;
        ensure!(matches.next().is_none(), "Release has duplicate assets");
        ensure!(
            found.size > 0 && found.size <= maximum,
            "Invalid release asset size"
        );
        validate_asset_url(&found.browser_download_url, &release.tag_name, name)?;
        Ok(found.clone())
    };
    Ok(Some(Release {
        version: version.to_string(),
        archive: asset(&name, MAX_ARCHIVE)?,
        checksum: asset(&format!("{name}.sha256"), 4096)?,
    }))
}

fn validate_asset_url(raw: &str, tag: &str, name: &str) -> Result<()> {
    let url = Url::parse(raw)?;
    let mut expected = Url::parse("https://github.com/oksyd/RustTerm/releases/download/")?;
    expected
        .path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .push(tag)
        .push(name);
    ensure!(
        url == expected,
        "Release asset must belong to the official GitHub release"
    );
    Ok(())
}

fn read_bounded(reader: impl Read, max: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(max + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= max, "Response exceeds the size limit");
    Ok(data)
}

fn checksum(text: &[u8], filename: &str) -> Result<[u8; 32]> {
    let text = std::str::from_utf8(text)?;
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(lines.len() == 1, "Expected one SHA-256 checksum");
    let mut fields = lines[0].split_whitespace();
    let hash = fields.next().unwrap_or_default();
    ensure!(
        fields.next().map(|name| name.trim_start_matches('*')) == Some(filename)
            && fields.next().is_none(),
        "Checksum filename does not match the release asset"
    );
    parse_hash(hash)
}

fn parse_hash(hash: &str) -> Result<[u8; 32]> {
    ensure!(
        hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid SHA-256 checksum"
    );
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16)?;
    }
    Ok(bytes)
}

/// All network/file work runs on a worker. Cancellation is honored until the
/// installer starts; the file transaction must then be allowed to finish.
pub fn download_and_install(
    release: &Release,
    proxy: &str,
    installation: &Installation,
    cancel: &UpdateControl,
    mut progress: impl FnMut(Progress),
) -> Result<()> {
    installation.validate()?;
    let agent = client(proxy, Duration::from_secs(20 * 60))?;
    let mut response = agent.get(&release.checksum.browser_download_url).call()?;
    let expected = checksum(
        &read_bounded(response.body_mut().as_reader(), 4096)?,
        &release.archive.name,
    )?;
    if let Some(digest) = &release.archive.digest {
        let digest = digest
            .strip_prefix("sha256:")
            .context("Unsupported release digest")?;
        ensure!(
            parse_hash(digest)? == expected,
            "GitHub digest and checksum file disagree"
        );
    }
    cancelled(cancel)?;
    let staging = tempfile::Builder::new()
        .prefix("rustterm-update-")
        .tempdir()?;
    let archive_path = staging.path().join("release.tar.gz");
    let mut archive = File::create(&archive_path)?;
    let mut response = agent.get(&release.archive.browser_download_url).call()?;
    copy_archive(
        response.body_mut().as_reader(),
        &mut archive,
        release.archive.size,
        expected,
        cancel,
        &mut progress,
    )?;
    archive.sync_all()?;
    drop(archive);
    progress(Progress::Verifying);
    cancelled(cancel)?;
    let package = install::unpack(&archive_path, staging.path(), release)?;
    cancelled(cancel)?;
    let _installing = cancel.begin_install()?;
    progress(Progress::Installing);
    installation.install(&package)?;
    Ok(())
}

fn cancelled(cancel: &UpdateControl) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Update cancelled");
    Ok(())
}

fn copy_archive(
    mut reader: impl Read,
    mut writer: impl Write,
    size: u64,
    expected: [u8; 32],
    cancel: &UpdateControl,
    progress: &mut impl FnMut(Progress),
) -> Result<()> {
    ensure!(
        size > 0 && size <= MAX_ARCHIVE,
        "Invalid release asset size"
    );
    let mut buffer = [0; 64 * 1024];
    let mut hash = Sha256::new();
    let mut received = 0;
    let mut last = Instant::now();
    progress(Progress::Download {
        received,
        total: size,
    });
    loop {
        cancelled(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received += count as u64;
        ensure!(
            received <= size,
            "Downloaded archive exceeds the advertised size"
        );
        hash.update(&buffer[..count]);
        writer.write_all(&buffer[..count])?;
        if last.elapsed() >= Duration::from_millis(150) {
            progress(Progress::Download {
                received,
                total: size,
            });
            last = Instant::now();
        }
    }
    ensure!(received == size, "Incomplete release download");
    ensure!(
        <[u8; 32]>::from(hash.finalize()) == expected,
        "Release archive SHA-256 verification failed"
    );
    Ok(())
}
