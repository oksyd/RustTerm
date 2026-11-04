//! Debian updates stay registered with the system package manager. Only APT is
//! elevated; release downloads and package checks run as the desktop user.
use super::*;
use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

fn package_version(version: &str) -> Result<String> {
    Version::parse(version)?;
    Ok(version.replace('-', "~"))
}

fn validate_status(status: &[u8], version: &str) -> Result<()> {
    let expected = format!(
        "install ok installed\n{}\namd64\n",
        package_version(version)?
    );
    ensure!(
        status == expected.as_bytes(),
        "The installed Debian package changed or is incomplete. Restart RustTerm after completing the system package operation."
    );
    Ok(())
}

pub(super) fn validate_installed(version: &str) -> Result<()> {
    let output = Command::new("/usr/bin/dpkg-query")
        .args([
            "--show",
            "--showformat=${Status}\n${Version}\n${Architecture}\n",
            "rustterm",
        ])
        .stdin(Stdio::null())
        .output()
        .context("Could not read the installed Debian package")?;
    ensure!(
        output.status.success(),
        "RustTerm is not registered with the system package manager"
    );
    validate_status(&output.stdout, version)
}

pub(super) fn verify_package(package: &Path, version: &str) -> Result<()> {
    let output = Command::new("/usr/bin/dpkg-deb")
        .arg("--field")
        .arg(package)
        .args(["Package", "Version", "Architecture"])
        .stdin(Stdio::null())
        .output()
        .context("Could not inspect the downloaded Debian package")?;
    ensure!(
        output.status.success(),
        "The downloaded file is not a valid Debian package"
    );
    let fields = std::str::from_utf8(&output.stdout)?;
    let mut fields: Vec<_> = fields.lines().collect();
    fields.sort_unstable();
    let version = format!("Version: {}", package_version(version)?);
    ensure!(
        fields == ["Architecture: amd64", "Package: rustterm", version.as_str()],
        "Debian package name, version or architecture does not match the release"
    );
    Ok(())
}

fn install_command(package: &Path, proxy: &str) -> Result<Command> {
    validate_proxy(proxy)?;
    ensure!(
        package.is_absolute(),
        "The Debian update path must be absolute"
    );
    let mut command = Command::new("/usr/bin/pkexec");
    command.args([
        "--disable-internal-agent",
        "/usr/bin/apt-get",
        "--assume-yes",
        "--no-remove",
        "--no-install-recommends",
        "-o",
        "DPkg::Lock::Timeout=120",
        "-o",
        "DPkg::Use-Pty=0",
        "-o",
        "Dpkg::Options::=--force-confold",
    ]);
    // APT may need new dependencies. Apply the same explicit proxy preference
    // to those downloads without changing the machine's APT configuration.
    let proxy = if proxy.is_empty() { "DIRECT" } else { proxy };
    command
        .arg("-o")
        .arg(format!("Acquire::http::Proxy={proxy}"));
    command
        .arg("-o")
        .arg(format!("Acquire::https::Proxy={proxy}"));
    command.arg("install").arg("--").arg(package);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    Ok(command)
}

fn check_install_result(output: &Output) -> Result<()> {
    match output.status.code() {
        Some(0) => Ok(()),
        Some(126) => bail!(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_AUTH_CANCELLED
        )),
        Some(127) => bail!(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_AUTH_UNAVAILABLE
        )),
        _ => bail!(
            "APT could not install the update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub(super) fn install(package: &Path, version: &str, proxy: &str) -> Result<()> {
    let output = install_command(package, proxy)?
        .output()
        .context(crate::i18n::Message::new(
            crate::i18n::messages::UPDATES_AUTH_UNAVAILABLE,
        ))?;
    check_install_result(&output)?;
    // A successful authentication is not sufficient: APT must have completed
    // the upgrade and recorded the expected version before offering Restart.
    validate_installed(version)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn installed_package_must_be_configured_at_the_expected_version() {
        validate_status(b"install ok installed\n0.2.0\namd64\n", "0.2.0").unwrap();
        for state in [
            "install ok unpacked\n0.2.0\namd64\n",
            "install ok installed\n0.3.0\namd64\n",
            "install ok installed\n0.2.0\narm64\n",
            "deinstall ok config-files\n0.2.0\namd64\n",
        ] {
            assert!(validate_status(state.as_bytes(), "0.2.0").is_err());
        }
        assert_eq!(
            package_version("1.2.3-rc.1+build.2").unwrap(),
            "1.2.3~rc.1+build.2"
        );
    }

    #[test]
    fn apt_uses_system_authorization_and_passes_paths_without_a_shell() {
        let path = Path::new("/tmp/update with spaces/$literal;file.deb");
        for proxy in ["", "http://127.0.0.1:7890", "https://proxy.example:8443"] {
            let command = install_command(path, proxy).unwrap();
            assert_eq!(command.get_program(), "/usr/bin/pkexec");
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(&args[..2], ["--disable-internal-agent", "/usr/bin/apt-get"]);
            assert_eq!(
                &args[args.len() - 3..],
                ["install", "--", path.to_str().unwrap()]
            );
            assert!(args.contains(&"--no-remove"));
            assert!(!args.contains(&"--allow-downgrades"));
            assert!(!args.contains(&"--allow-change-held-packages"));
            let proxy = if proxy.is_empty() { "DIRECT" } else { proxy };
            for protocol in ["http", "https"] {
                assert!(args.contains(&format!("Acquire::{protocol}::Proxy={proxy}").as_str()));
            }
        }
        assert!(install_command(path, "http://user:password@proxy.example").is_err());
        assert!(install_command(Path::new("relative.deb"), "").is_err());
    }

    #[test]
    fn cancelled_authorization_and_apt_failure_are_not_successful_updates() {
        for code in [0, 100, 126, 127] {
            let output = Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: b"package manager error".to_vec(),
            };
            assert_eq!(check_install_result(&output).is_ok(), code == 0);
            if code == 126 {
                let error = check_install_result(&output).unwrap_err();
                assert!(error.downcast_ref::<crate::i18n::Message>().is_some());
            }
        }
    }

    #[test]
    fn downloaded_debian_package_identity_is_checked_before_authorization() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        std::fs::create_dir_all(root.join("DEBIAN")).unwrap();
        let package = temporary.path().join("update.deb");
        for (name, version, arch, valid) in [
            ("rustterm", "0.2.0", "amd64", true),
            ("other-package", "0.2.0", "amd64", false),
            ("rustterm", "0.1.0", "amd64", false),
            ("rustterm", "0.2.0", "arm64", false),
        ] {
            std::fs::write(root.join("DEBIAN/control"), format!(
                "Package: {name}\nVersion: {version}\nArchitecture: {arch}\nMaintainer: RustTerm\nDescription: Test fixture\n"
            )).unwrap();
            assert!(
                Command::new("/usr/bin/dpkg-deb")
                    .args(["--root-owner-group", "--build"])
                    .arg(&root)
                    .arg(&package)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(verify_package(&package, "0.2.0").is_ok(), valid);
        }
    }
}
