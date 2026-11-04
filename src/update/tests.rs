use super::*;
use std::os::unix::fs::DirBuilderExt;
use std::{fs, io::Cursor, path::Path};

fn release_json(version: &str) -> GithubRelease {
    let name = format!(
        "rustterm-{}-linux-x86_64.tar.gz",
        version.trim_start_matches('v')
    );
    serde_json::from_value(serde_json::json!({
        "tag_name": version, "draft": false, "prerelease": false,
        "assets": [
            {"name": name, "size": 120, "browser_download_url": format!("{RELEASES_URL}/download/{version}/{name}")},
            {"name": format!("{name}.sha256"), "size": 120, "browser_download_url": format!("{RELEASES_URL}/download/{version}/{name}.sha256")}
        ]
    })).unwrap()
}
fn release() -> Release {
    select_release(release_json("0.2.0"), "0.1.0")
        .unwrap()
        .unwrap()
}

#[test]
fn stable_semantic_versions_only_and_never_downgrade() {
    for (tag, current, expected) in [
        ("0.2.0", "0.1.0", true),
        ("v0.2.0", "0.1.0", true),
        ("0.10.0", "0.9.0", true),
        ("v0.1.0", "0.1.0", false),
        ("v0.1.0", "0.2.0", false),
        ("v0.1.0+other", "0.1.0+local", false),
        ("v0.2.0-rc.1", "0.1.0", false),
    ] {
        assert_eq!(
            select_release(release_json(tag), current)
                .unwrap()
                .is_some(),
            expected
        );
    }
    let mut draft = release_json("0.2.0");
    draft.draft = true;
    assert!(select_release(draft, "0.1.0").unwrap().is_none());
    let mut pre = release_json("0.2.0");
    pre.prerelease = true;
    assert!(select_release(pre, "0.1.0").unwrap().is_none());
    assert!(select_release(release_json("latest"), "0.1.0").is_err());
}

#[test]
fn reject_incomplete_ambiguous_and_external_release_assets() {
    let mut missing = release_json("v0.2.0");
    missing.assets.pop();
    assert!(select_release(missing, "0.1.0").is_err());
    let mut wrong_arch = release_json("v0.2.0");
    wrong_arch.assets[0].name = "rustterm-0.2.0-linux-aarch64.tar.gz".into();
    assert!(select_release(wrong_arch, "0.1.0").is_err());
    let mut duplicate = release_json("v0.2.0");
    duplicate.assets.push(duplicate.assets[0].clone());
    assert!(select_release(duplicate, "0.1.0").is_err());
    for url in [
        "http://github.com/oksyd/RustTerm/releases/download/v0.2.0/a",
        "https://example.com/file",
        "https://github.com/other/repo/releases/download/v0.2.0/a",
    ] {
        let mut external = release_json("v0.2.0");
        external.assets[0].browser_download_url = url.into();
        assert!(select_release(external, "0.1.0").is_err());
    }
    let mut huge = release_json("v0.2.0");
    huge.assets[0].size = MAX_ARCHIVE + 1;
    assert!(select_release(huge, "0.1.0").is_err());
}

#[test]
fn proxy_validation_and_saved_preferences() {
    for proxy in [
        "",
        "http://127.0.0.1:7890",
        "https://proxy.example:8443",
        "http://[::1]:8080/",
    ] {
        validate_proxy(proxy).unwrap();
    }
    for proxy in [
        "127.0.0.1:7890",
        "socks5://localhost:1080",
        "http://user:pass@host:8080",
        "http://host/path",
        "http://host?query",
        "http://host#fragment",
        "http://host:0",
        " http://host:8080",
        "http://host:70000",
        "http://host\n",
    ] {
        assert!(validate_proxy(proxy).is_err(), "{proxy}");
    }
    let mut old: crate::config::Settings = toml::from_str("font_size = 15.0").unwrap();
    assert!(old.check_updates && old.update_proxy.is_empty());
    old.check_updates = false;
    old.update_proxy = "http://127.0.0.1:7890".into();
    let saved: crate::config::Settings = toml::from_str(&toml::to_string(&old).unwrap()).unwrap();
    assert!(!saved.check_updates);
    assert_eq!(old.update_proxy, saved.update_proxy);
    saved.validate().unwrap();
}

#[test]
fn https_uses_configured_connect_proxy_and_direct_ignores_environment() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("CONNECT api.github.com:443 HTTP/1.1\r\n")
        );
        socket
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let agent = client(&format!("http://{address}"), Duration::from_secs(5)).unwrap();
    assert!(agent.get(LATEST_URL).call().is_err());
    server.join().unwrap();
    let direct = client("", Duration::from_secs(1)).unwrap();
    assert!(direct.config().proxy().is_none());
    assert!(direct.get("http://127.0.0.1:1").call().is_err());
}

#[test]
fn checksum_filename_and_download_integrity_are_enforced() {
    let data = b"release bytes";
    let hash: [u8; 32] = Sha256::digest(data).into();
    let hex = format!("{:x}", Sha256::digest(data));
    assert_eq!(
        checksum(
            format!("{hex}  release.tar.gz\n").as_bytes(),
            "release.tar.gz"
        )
        .unwrap(),
        hash
    );
    for text in [
        format!("{hex}  wrong.tar.gz"),
        format!("{hex}"),
        format!("{hex}  release.tar.gz\n{hex} release.tar.gz"),
        format!("{}  release.tar.gz", "é".repeat(32)),
    ] {
        assert!(checksum(text.as_bytes(), "release.tar.gz").is_err());
    }
    let cancel = UpdateControl::default();
    let mut output = Vec::new();
    copy_archive(
        Cursor::new(data),
        &mut output,
        data.len() as u64,
        hash,
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output, data);
    for (size, expected) in [
        (data.len() as u64 + 1, hash),
        (data.len() as u64 - 1, hash),
        (data.len() as u64, [0; 32]),
    ] {
        assert!(
            copy_archive(
                Cursor::new(data),
                Vec::new(),
                size,
                expected,
                &cancel,
                &mut |_| {}
            )
            .is_err()
        );
    }
    cancel.cancel();
    assert!(
        copy_archive(
            Cursor::new(data),
            Vec::new(),
            data.len() as u64,
            hash,
            &cancel,
            &mut |_| {}
        )
        .is_err()
    );
    assert!(read_bounded(Cursor::new(data), 3).is_err());
}

fn append(tar: &mut tar::Builder<impl Write>, name: &str, data: &[u8], kind: tar::EntryType) {
    let mut header = tar::Header::new_gnu();
    header.set_path(name).unwrap();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(kind);
    if kind.is_symlink() {
        header.set_link_name("/tmp/outside").unwrap();
    }
    header.set_cksum();
    tar.append(&header, data).unwrap();
}

fn archive(
    path: &Path,
    variant: &str,
    version: &str,
    extra: Option<(&str, tar::EntryType)>,
) -> std::path::PathBuf {
    let target = path.join("test.tar.gz");
    let gzip =
        flate2::write::GzEncoder::new(File::create(&target).unwrap(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    let binary = b"#!/bin/sh\nprintf 'updated\\n'\n";
    let manifest = serde_json::json!({"platform": "linux-x86_64", "variant": variant, "version": version, "binary_sha256": format!("{:x}", Sha256::digest(binary))}).to_string();
    let root = "rustterm-0.2.0-linux-x86_64";
    for (name, data) in [
        ("bin/rustterm", binary.as_slice()),
        ("build-info.json", manifest.as_bytes()),
        (
            "share/applications/io.rustterm.RustTerm.desktop",
            b"[Desktop Entry]\nExec=rustterm\n".as_slice(),
        ),
        (
            "share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg",
            b"icon".as_slice(),
        ),
        ("share/doc/rustterm/LICENSE", b"license".as_slice()),
        ("share/doc/rustterm/README.md", b"readme".as_slice()),
        (
            "share/doc/rustterm/alacritty_terminal-LICENSE-APACHE",
            b"notice".as_slice(),
        ),
        // This must never be executed, even with a valid archive checksum.
        (
            "install.py",
            b"raise RuntimeError('untrusted script ran')".as_slice(),
        ),
    ] {
        append(
            &mut tar,
            &format!("{root}/{name}"),
            data,
            tar::EntryType::Regular,
        );
    }
    if let Some((name, kind)) = extra {
        append(&mut tar, name, b"", kind);
    }
    tar.into_inner().unwrap().finish().unwrap();
    target
}

#[test]
fn archive_rejects_links_duplicate_files_wrong_root_and_manifest_mismatch() {
    for (variant, version, extra) in [
        ("dev", "0.2.0", None),
        ("release", "0.3.0", None),
        (
            "release",
            "0.2.0",
            Some(("outside/file", tar::EntryType::Regular)),
        ),
        (
            "release",
            "0.2.0",
            Some((
                "rustterm-0.2.0-linux-x86_64/bin/rustterm",
                tar::EntryType::Regular,
            )),
        ),
        (
            "release",
            "0.2.0",
            Some(("rustterm-0.2.0-linux-x86_64/link", tar::EntryType::Symlink)),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive(dir.path(), variant, version, extra);
        assert!(install::unpack(&archive, dir.path(), &release()).is_err());
    }
}

#[test]
fn debian_installation_is_not_managed_by_the_user_prefix_updater() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path();
    fs::create_dir(prefix.join("bin")).unwrap();
    fs::create_dir_all(prefix.join("share/doc/rustterm")).unwrap();
    fs::write(prefix.join("bin/rustterm"), b"installed binary").unwrap();
    let marker = prefix.join("share/doc/rustterm/build-info.json");
    fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "platform": "linux-x86_64", "variant": "release",
            "version": CURRENT_VERSION, "installation": "deb",
        }))
        .unwrap(),
    )
    .unwrap();
    let error = Installation::at_executable(&prefix.join("bin/rustterm")).unwrap_err();
    assert!(error.to_string().contains(".deb with APT"));
    assert_eq!(
        fs::read(prefix.join("bin/rustterm")).unwrap(),
        b"installed binary"
    );
}

#[test]
fn verified_package_updates_custom_prefix_without_running_downloaded_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let archive = archive(dir.path(), "release", "0.2.0", None);
    let package = install::unpack(&archive, dir.path(), &release()).unwrap();
    assert!(!package.join("install.py").exists());
    let prefix = dir.path().join("my installation");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(prefix.join("bin"))
        .unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(prefix.join("share/doc/rustterm"))
        .unwrap();
    fs::write(prefix.join("bin/rustterm"), b"old binary").unwrap();
    fs::write(
        prefix.join("share/doc/rustterm/build-info.json"),
        serde_json::to_vec(&serde_json::json!({"platform": "linux-x86_64", "variant": "release", "version": CURRENT_VERSION})).unwrap(),
    )
    .unwrap();
    fs::write(prefix.join("keep-config"), b"untouched").unwrap();
    let installation = Installation::at_executable(&prefix.join("bin/rustterm")).unwrap();
    installation.install(&package).unwrap();
    assert_eq!(fs::read(prefix.join("keep-config")).unwrap(), b"untouched");
    let result = std::process::Command::new(installation.executable())
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, b"updated\n");
    let desktop =
        fs::read_to_string(prefix.join("share/applications/io.rustterm.RustTerm.desktop")).unwrap();
    assert!(desktop.contains("my installation/bin/rustterm"));
    assert!(Installation::at_executable(&dir.path().join("target/debug/rustterm")).is_err());
}

#[test]
fn cancelling_and_starting_installation_are_mutually_exclusive() {
    let cancelled = UpdateControl::default();
    assert!(cancelled.cancel());
    assert!(cancelled.begin_install().is_err());
    let installing = UpdateControl::default();
    let guard = installing.begin_install().unwrap();
    assert!(!installing.can_cancel());
    assert!(!installing.cancel());
    drop(guard);
    assert!(installing.cancel()); // Shutdown is safe after commit or rollback.
    assert!(!installing.is_cancelled());
}
