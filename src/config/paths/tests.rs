use super::*;
use crate::config::{ConfigLock, PanePosition, SavedLayout, SavedTab, SavedWorkspace};
use std::fs;

fn fixture(root: &Path) -> AppPaths {
    AppPaths {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
    }
}

fn with_workspace() -> AppConfig {
    let mut config = AppConfig::default();
    config.workspace = WorkspaceSnapshot {
        layout: SavedWorkspace {
            tabs: vec![SavedTab {
                title: "Local".into(),
                layout: SavedLayout::Pane { index: 0 },
            }],
            ..Default::default()
        },
        panes: vec![PanePosition {
            profile: config.profiles[0].id,
            ..Default::default()
        }],
    };
    config
}

#[test]
fn xdg_ignores_relative_and_empty_values_and_requires_an_absolute_fallback() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let override_path = root.path().join("override");
    for fallback in [".config", ".local/state", ".cache"] {
        for value in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                xdg_base(value, Some(&home), fallback).unwrap(),
                home.join(fallback)
            );
            assert!(xdg_base(value, Some(Path::new("relative-home")), fallback).is_err());
            assert!(xdg_base(value, None, fallback).is_err());
        }
        assert_eq!(
            xdg_base(Some(&override_path), None, fallback).unwrap(),
            override_path
        );
    }
}

#[test]
fn custom_profiles_have_independent_state_cache_and_locks() {
    let root = tempfile::tempdir().unwrap();
    let first = AppPaths::discover(Some(root.path().join("first"))).unwrap();
    let second = AppPaths::discover(Some(root.path().join("second"))).unwrap();
    assert_eq!(first.state, first.config.join("state"));
    assert_eq!(first.cache, first.config.join("cache"));
    let _first_lock = ConfigLock::acquire(&first.config).unwrap();
    let _second_lock = ConfigLock::acquire(&second.config).unwrap();
    assert!(matches!(
        ConfigLock::acquire(&first.config),
        Err(ConfigError::AlreadyRunning)
    ));
    let config = with_workspace();
    first.save_config(&config).unwrap();
    second.save_config(&AppConfig::default()).unwrap();
    assert_eq!(first.load_config().unwrap().workspace, config.workspace);
    assert!(second.load_config().unwrap().workspace.is_empty());
    let relative = AppPaths::discover(Some(PathBuf::from("profile-fixture"))).unwrap();
    assert_eq!(
        relative.config,
        std::env::current_dir().unwrap().join("profile-fixture")
    );
}

#[test]
fn migrates_inline_workspace_without_changing_profiles_or_trust_records() {
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    let _lock = ConfigLock::acquire(&paths.config).unwrap();
    let config = with_workspace();
    let path = paths.config.join("config.toml");
    config.save(&path).unwrap();
    let trust = paths.config.join("known_hosts.toml");
    storage::write_atomic(&trust, b"fixture trust data").unwrap();
    for _ in 0..2 {
        let restored = paths.load_config().unwrap();
        assert_eq!(restored.workspace, config.workspace);
        assert_eq!(restored.profiles[0].id, config.profiles[0].id);
        assert_eq!(fs::read(&trust).unwrap(), b"fixture trust data");
    }
    assert!(paths.state.join("workspace.toml").is_file());
    assert!(AppConfig::load(&path).unwrap().workspace.is_empty());
    assert!(!fs::read_to_string(&path).unwrap().contains("[workspace"));
}

#[test]
fn existing_empty_snapshot_wins_over_legacy_open_tabs() {
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    paths.save_config(&AppConfig::default()).unwrap();
    with_workspace()
        .save(paths.config.join("config.toml"))
        .unwrap();
    assert!(paths.load_config().unwrap().workspace.is_empty());
    assert!(
        AppConfig::load(paths.config.join("config.toml"))
            .unwrap()
            .workspace
            .is_empty()
    );
}

#[test]
fn failed_state_migration_leaves_original_config_and_can_be_retried() {
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    let config = with_workspace();
    let path = paths.config.join("config.toml");
    config.save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    fs::write(&paths.state, b"not a directory").unwrap();
    let error = paths.load_config().unwrap_err();
    assert!(!error.was_committed());
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::remove_file(&paths.state).unwrap();
    assert_eq!(paths.load_config().unwrap().workspace, config.workspace);
}

#[test]
fn invalid_state_is_not_replaced_by_legacy_state() {
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    with_workspace()
        .save(paths.config.join("config.toml"))
        .unwrap();
    let config_before = fs::read(paths.config.join("config.toml")).unwrap();
    let state = paths.state.join("workspace.toml");
    storage::write_atomic(&state, b"invalid = [").unwrap();
    assert!(paths.load_config().is_err());
    assert_eq!(fs::read(&state).unwrap(), b"invalid = [");
    assert_eq!(
        fs::read(paths.config.join("config.toml")).unwrap(),
        config_before
    );
}

#[test]
fn failed_state_save_does_not_commit_new_credential_references() {
    use crate::config::{AuthMethod, CredentialRef, Profile, SshConfig};
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    let mut config = AppConfig::default();
    config.save(paths.config.join("config.toml")).unwrap();
    let original = fs::read(paths.config.join("config.toml")).unwrap();
    config.profiles.push(Profile::ssh(
        "Server",
        SshConfig {
            host: "example.invalid".into(),
            username: "fixture".into(),
            auth: AuthMethod::Password {
                credential: Some(CredentialRef::new()),
            },
            ..Default::default()
        },
    ));
    fs::write(&paths.state, b"not a directory").unwrap();
    assert!(!paths.save_config(&config).unwrap_err().was_committed());
    assert_eq!(
        fs::read(paths.config.join("config.toml")).unwrap(),
        original
    );
    fs::remove_file(&paths.state).unwrap();
    paths.save_config(&config).unwrap();
    assert_eq!(
        paths.load_config().unwrap().profiles[1]
            .connection
            .credential_ref(),
        config.profiles[1].connection.credential_ref()
    );
}

#[test]
fn moves_old_logs_without_overwriting_newer_logs_and_is_repeatable() {
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    for name in ["diagnostics.log", "diagnostics.1.log", "diagnostics.2.log"] {
        storage::write_atomic(&paths.config.join(name), b"old log").unwrap();
    }
    storage::write_atomic(&paths.logs().join("diagnostics.log"), b"new log").unwrap();
    for _ in 0..2 {
        paths.prepare_logs().unwrap();
    }
    assert_eq!(
        fs::read(paths.logs().join("diagnostics.log")).unwrap(),
        b"new log"
    );
    assert_eq!(
        fs::read(paths.config.join("diagnostics.log")).unwrap(),
        b"old log"
    );
    for name in ["diagnostics.1.log", "diagnostics.2.log"] {
        assert_eq!(fs::read(paths.logs().join(name)).unwrap(), b"old log");
        assert!(!paths.config.join(name).exists());
    }
}

#[cfg(unix)]
#[test]
fn creates_private_state_and_logs_and_rejects_unsafe_storage() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let paths = fixture(root.path());
    paths.save_config(&with_workspace()).unwrap();
    storage::write_atomic(&paths.config.join("diagnostics.log"), b"log").unwrap();
    paths.prepare_logs().unwrap();
    for dir in [&paths.config, &paths.state, &paths.logs()] {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for file in [
        paths.state.join("workspace.toml"),
        paths.logs().join("diagnostics.log"),
    ] {
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let config_before = fs::read(paths.config.join("config.toml")).unwrap();
    let state = paths.state.join("workspace.toml");
    fs::remove_file(&state).unwrap();
    let outside = root.path().join("untouched");
    fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, &state).unwrap();
    assert!(
        !paths
            .save_config(&with_workspace())
            .unwrap_err()
            .was_committed()
    );
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    fs::remove_file(state).unwrap();
    fs::set_permissions(&paths.state, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        !paths
            .save_config(&with_workspace())
            .unwrap_err()
            .was_committed()
    );
    assert_eq!(
        fs::read(paths.config.join("config.toml")).unwrap(),
        config_before
    );
}
