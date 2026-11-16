use super::*;
use std::io::Cursor;
use zip::write::SimpleFileOptions;

fn manifest(version: &str, binary: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": version, "platform": "windows-x86_64", "variant": "release",
        "binary_sha256": format!("{:x}", Sha256::digest(binary)),
    }))
    .unwrap()
}

fn payload(version: &str) -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = FILES
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                format!("{name} {version}").into_bytes(),
            )
        })
        .collect();
    let mut binary = vec![0; 128];
    binary[..2].copy_from_slice(b"MZ");
    binary[60..64].copy_from_slice(&64u32.to_le_bytes());
    binary[64..70].copy_from_slice(b"PE\0\0\x64\x86");
    binary.extend_from_slice(version.as_bytes());
    files.insert(BINARY.into(), binary);
    files.insert(MANIFEST.into(), manifest(version, &files[BINARY]));
    files
}

fn write_payload(directory: &Path, version: &str) {
    fs::create_dir_all(directory).unwrap();
    for (name, bytes) in payload(version) {
        fs::write(directory.join(name), bytes).unwrap();
    }
}

fn release() -> Release {
    let name = "rustterm-99.0.0-windows-x86_64.zip";
    Release {
        version: "99.0.0".into(),
        archive: Asset {
            name: name.into(),
            size: 1,
            browser_download_url: String::new(),
            digest: None,
        },
        checksum: Asset {
            name: format!("{name}.sha256"),
            size: 1,
            browser_download_url: String::new(),
            digest: None,
        },
    }
}

fn zip_bytes(files: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(
                name,
                SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .large_file(true),
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn entries() -> Vec<(String, Vec<u8>)> {
    payload("99.0.0")
        .into_iter()
        .map(|(name, bytes)| (format!("rustterm-99.0.0-windows-x86_64/{name}"), bytes))
        .collect()
}

fn unpack_bytes(bytes: Vec<u8>) -> Result<tempfile::TempDir> {
    let temporary = tempfile::tempdir()?;
    let archive = temporary.path().join("download.zip");
    fs::write(&archive, bytes)?;
    unpack(&archive, temporary.path(), &release())?;
    Ok(temporary)
}

#[test]
fn zip_validates_payload_and_rejects_paths_aliases_links_and_corruption() {
    unpack_bytes(zip_bytes(entries())).unwrap();
    for bad in [
        "../escape",
        "rustterm.exe:stream",
        "rustterm.exe.",
        "RUSTTERM.EXE",
        "folder/rustterm.exe",
        "folder\\rustterm.exe",
        "C:rustterm.exe",
    ] {
        let mut files = entries();
        files.push((
            format!("rustterm-99.0.0-windows-x86_64/{bad}"),
            b"bad".to_vec(),
        ));
        assert!(unpack_bytes(zip_bytes(files)).is_err(), "accepted {bad}");
    }
    let mut missing = entries();
    missing.pop();
    assert!(unpack_bytes(zip_bytes(missing)).is_err());
    let mut corrupt = entries();
    corrupt
        .iter_mut()
        .find(|(name, _)| name.ends_with(BINARY))
        .unwrap()
        .1
        .push(0);
    assert!(unpack_bytes(zip_bytes(corrupt)).is_err());
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_symlink(
            "rustterm-99.0.0-windows-x86_64/rustterm.exe",
            "outside",
            SimpleFileOptions::default(),
        )
        .unwrap();
    assert!(unpack_bytes(writer.finish().unwrap().into_inner()).is_err());
    // Central directory names must not create aliases for an already seen file.
    let mut duplicate = entries();
    duplicate.push((
        "rustterm-99.0.0-windows-x86_64/XXXXXXXX.exe".into(),
        b"bad".to_vec(),
    ));
    let mut bytes = zip_bytes(duplicate);
    for offset in 0..bytes.len().saturating_sub(12) {
        if &bytes[offset..offset + 12] == b"XXXXXXXX.exe" {
            bytes[offset..offset + 12].copy_from_slice(BINARY.as_bytes());
        }
    }
    assert!(unpack_bytes(bytes).is_err());
}

#[test]
fn portable_installation_rejects_dev_versions_and_modified_executables() {
    let directory = tempfile::tempdir().unwrap();
    write_payload(directory.path(), CURRENT_VERSION);
    validate_installation(directory.path(), CURRENT_VERSION).unwrap();
    assert!(validate_installation(directory.path(), "0.0.0").is_err());
    fs::write(directory.path().join(BINARY), b"modified").unwrap();
    assert!(validate_installation(directory.path(), CURRENT_VERSION).is_err());
    write_payload(directory.path(), CURRENT_VERSION);
    let path = directory.path().join(MANIFEST);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["variant"] = "dev".into();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(validate_installation(directory.path(), CURRENT_VERSION).is_err());
}

fn prepared() -> (tempfile::TempDir, Prepared) {
    let root = tempfile::tempdir().unwrap();
    let installed = root.path().join("portable folder 中文");
    let package = root.path().join("new");
    write_payload(&installed, CURRENT_VERSION);
    write_payload(&package, "99.0.0");
    let marker = validate_installation(&installed, CURRENT_VERSION).unwrap();
    let prepared = prepare(&installed, &package, "99.0.0", marker).unwrap();
    (root, prepared)
}

#[test]
fn transaction_preserves_user_files_and_restores_every_file_after_failure() {
    for failed_at in 0..FILES.len() {
        let (_root, prepared) = prepared();
        let directory = prepared.directory.path();
        let prefix = directory.parent().unwrap();
        fs::write(prefix.join("user-data.txt"), b"keep me").unwrap();
        assert!(
            apply_with(directory, |index| {
                ensure!(index != failed_at, "simulated locked file");
                Ok(())
            })
            .is_err()
        );
        for (name, bytes) in payload(CURRENT_VERSION) {
            assert_eq!(fs::read(prefix.join(name)).unwrap(), bytes);
        }
        assert_eq!(fs::read(prefix.join("user-data.txt")).unwrap(), b"keep me");
        assert!(!directory.join("applying").exists());
        apply(directory).unwrap();
        for (name, bytes) in payload("99.0.0") {
            assert_eq!(fs::read(prefix.join(name)).unwrap(), bytes);
        }
    }
}

#[test]
fn interrupted_replacement_recovers_and_tampered_staging_never_changes_installation() {
    let (_root, prepared) = prepared();
    let directory = prepared.directory.path();
    let prefix = directory.parent().unwrap();
    fs::write(directory.join("applying"), b"").unwrap();
    fs::copy(directory.join("new/README.md"), prefix.join("README.md")).unwrap();
    apply(directory).unwrap();
    validate_installation(prefix, "99.0.0").unwrap();

    let (_root, prepared) = self::prepared();
    let directory = prepared.directory.path();
    fs::write(directory.join("new/README.md"), b"tampered").unwrap();
    assert!(apply(directory).is_err());
    validate_installation(directory.parent().unwrap(), CURRENT_VERSION).unwrap();
    assert!(!directory.join("applying").exists());
}

#[test]
fn preparation_is_exclusive_and_closing_without_restart_removes_staging() {
    let (_root, prepared) = prepared();
    let path = prepared.directory.path().to_path_buf();
    let prefix = path.parent().unwrap().to_path_buf();
    assert!(lock(&prefix).is_err());
    drop(prepared);
    assert!(!path.exists());
    lock(&prefix).unwrap();
}

#[test]
fn archive_crc_errors_are_detected() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries() {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    let mut bytes = writer.finish().unwrap().into_inner();
    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    let body = archive
        .by_name("rustterm-99.0.0-windows-x86_64/LICENSE")
        .unwrap()
        .data_start()
        .unwrap() as usize;
    drop(archive);
    // Change stored data, leaving both the manifest and CRC unchanged.
    bytes[body] ^= 1;
    assert!(unpack_bytes(bytes).is_err());
}

#[cfg(windows)]
#[test]
fn locked_executable_rolls_back_then_allows_retry() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_root, prepared) = prepared();
    let directory = prepared.directory.path();
    let prefix = directory.parent().unwrap();
    // An open executable in another process prevents replacement on Windows.
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(prefix.join(BINARY))
        .unwrap();
    assert!(apply(directory).is_err());
    for (name, bytes) in payload(CURRENT_VERSION) {
        assert_eq!(fs::read(prefix.join(name)).unwrap(), bytes);
    }
    drop(locked);
    apply(directory).unwrap();
    validate_installation(prefix, "99.0.0").unwrap();
}

#[cfg(windows)]
#[test]
fn failed_post_install_rollback_remains_recoverable() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_root, prepared) = prepared();
    let directory = prepared.directory.path();
    let prefix = directory.parent().unwrap();
    apply(directory).unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(prefix.join(BINARY))
        .unwrap();
    assert!(undo(directory).is_err());
    assert!(directory.join("applying").exists());
    assert!(!directory.join("complete").exists());
    drop(locked);
    apply(directory).unwrap();
    validate_installation(prefix, "99.0.0").unwrap();
}
