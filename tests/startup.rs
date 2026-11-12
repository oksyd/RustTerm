//! Test failures before GPUI initialization with desktop utilities that record
//! argv. No display or real user configuration is required.
#![cfg(target_os = "linux")]

use rustterm::config::{AppConfig, AppPaths, ConfigLock};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    paths: AppPaths,
    tools: PathBuf,
    trace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths =
            AppPaths::discover(Some(root.path().join("profile with <markup> and 'quotes'")))
                .unwrap();
        AppConfig::default()
            .save(paths.config.join("config.toml"))
            .unwrap();
        let tools = root.path().join("tools");
        fs::create_dir(&tools).unwrap();
        let trace = root.path().join("dialog");
        let fixture = Self {
            root,
            paths,
            tools,
            trace,
        };
        fixture.dialog_tool("zenity");
        fixture
    }

    fn dialog_tool(&self, name: &str) {
        let tool = self.tools.join(name);
        fs::write(
            &tool,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$RUSTTERM_TEST_DIALOG_TRACE\"\n",
        )
        .unwrap();
        fs::set_permissions(tool, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_config(&self, contents: &str) {
        fs::write(self.paths.config.join("config.toml"), contents).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rustterm"));
        command
            .env("PATH", &self.tools)
            .env("DISPLAY", ":rustterm-startup-test")
            .env_remove("WAYLAND_DISPLAY")
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8")
            .env("LANGUAGE", "en")
            .env("RUSTTERM_TEST_DIALOG_TRACE", &self.trace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn launch(&self) -> Output {
        let mut command = self.command();
        command.arg("--config-dir").arg(&self.paths.config);
        bounded_output(&mut command)
    }

    fn dialog(&self) -> String {
        fs::read_to_string(&self.trace).unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.paths.logs().join("diagnostics.log")).unwrap()
    }
}

fn bounded_output(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("startup failure did not exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn incompatible_config_shows_a_plain_native_error_and_is_not_overwritten() {
    for source in [
        "schema_version = 999\nsecret = 'do-not-echo-secret'\n",
        "schema_version = 1\n'do-not-echo-secret' = true\n",
    ] {
        let fixture = Fixture::new();
        fixture.write_config(source);
        let output = fixture.launch();
        assert!(!output.status.success());
        let dialog = fixture.dialog();
        assert!(dialog.contains("--no-markup\0"));
        assert!(dialog.contains(fixture.paths.config.to_str().unwrap()));
        assert!(dialog.contains(env!("CARGO_PKG_VERSION")));
        assert!(!dialog.contains("do-not-echo-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("do-not-echo-secret"));
        assert_eq!(
            fs::read_to_string(fixture.paths.config.join("config.toml")).unwrap(),
            source
        );
        let log = fixture.log();
        assert!(log.contains("startup-failed") && log.contains("incompatible-config"));
        assert!(
            !log.contains("do-not-echo-secret")
                && !log.contains(fixture.root.path().to_str().unwrap())
        );
    }
}

#[test]
fn malformed_workspace_identifies_the_state_file_and_preserves_both_files() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.paths.config.join("config.toml")).unwrap();
    fs::create_dir_all(&fixture.paths.state).unwrap();
    fs::set_permissions(&fixture.paths.state, fs::Permissions::from_mode(0o700)).unwrap();
    let state = fixture.paths.state.join("workspace.toml");
    fs::write(&state, "not valid = ['do-not-echo-secret'").unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!fixture.launch().status.success());
    assert!(fixture.dialog().contains("workspace.toml"));
    assert!(!fixture.dialog().contains("do-not-echo-secret"));
    assert!(fixture.log().contains("invalid-config"));
    assert_eq!(
        fs::read(fixture.paths.config.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(state).unwrap(),
        "not valid = ['do-not-echo-secret'"
    );
}

#[test]
fn shared_profile_lock_reports_the_running_instance_before_loading_config() {
    let fixture = Fixture::new();
    fixture.write_config("schema_version = 999");
    let _lock = ConfigLock::acquire(&fixture.paths.config).unwrap();
    assert!(!fixture.launch().status.success());
    let dialog = fixture.dialog();
    assert!(dialog.contains("Another RustTerm instance"));
    assert!(dialog.contains("--config-dir"));
    assert!(
        !fixture.paths.logs().exists(),
        "a competing instance must not migrate logs"
    );
}

#[test]
fn headless_failures_log_synchronously_without_attempting_a_desktop_dialog() {
    let fixture = Fixture::new();
    fixture.write_config("schema_version = 999");
    let mut command = fixture.command();
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--config-dir")
        .arg(&fixture.paths.config);
    assert!(!bounded_output(&mut command).status.success());
    assert!(!fixture.trace.exists());
    assert!(fixture.log().contains("incompatible-config"));
}

#[test]
fn missing_dialog_utility_uses_a_desktop_notification() {
    let fixture = Fixture::new();
    fixture.write_config("schema_version = 999");
    fs::remove_file(fixture.tools.join("zenity")).unwrap();
    fixture.dialog_tool("notify-send");
    assert!(!fixture.launch().status.success());
    assert!(fixture.dialog().contains("--urgency=critical\0"));
}

#[test]
fn help_version_and_credential_worker_do_not_open_startup_dialogs() {
    let fixture = Fixture::new();
    for argument in ["--help", "--version", "--credential-worker"] {
        let output = bounded_output(fixture.command().arg(argument));
        assert_eq!(output.status.success(), argument != "--credential-worker");
        assert!(!fixture.trace.exists());
    }
}
