use super::*;

#[test]
fn sql_profiles_preserve_engine_routes_and_credential_references() {
    let ssh = Profile::ssh(
        "Gateway",
        SshConfig {
            host: "gateway.example.invalid".into(),
            username: "operator".into(),
            ..SshConfig::default()
        },
    );
    let reference = CredentialRef::new();
    let mut mysql = SqlConfig::mysql();
    assert_eq!(mysql.port, 3306);
    assert!(mysql.database.is_empty());
    mysql.ssh_profile = Some(ssh.id);
    mysql.credential = Some(reference);
    mysql.tls = true;
    let postgres = SqlConfig::postgres();
    assert_eq!(
        (postgres.port, postgres.database.as_str()),
        (5432, "postgres")
    );
    let config = AppConfig {
        profiles: vec![
            ssh.clone(),
            Profile::mysql("MySQL", mysql.clone()),
            Profile::postgres("PostgreSQL", postgres.clone()),
        ],
        ..AppConfig::default()
    };
    config.validate().unwrap();
    assert!(config.uses_ssh_profile(ssh.id));
    assert!(config.uses_credential(reference));
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("type = \"mysql\""));
    assert!(encoded.contains("type = \"postgres\""));
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.profiles[1].connection, Connection::Mysql(mysql));
    assert_eq!(
        decoded.profiles[2].connection,
        Connection::Postgres(postgres)
    );
    let mut missing_gateway = decoded;
    missing_gateway.profiles.remove(0);
    assert!(missing_gateway.validate().is_err());
}

#[test]
fn telnet_profiles_round_trip_without_credentials() {
    let telnet: TelnetConfig = toml::from_str("host = 'console.example.invalid'").unwrap();
    assert_eq!(telnet.port, 23);
    assert_eq!(telnet.timeout_secs, 15);
    let profile = Profile::telnet("Console", telnet.clone());
    let profile_id = profile.id;
    let config = AppConfig {
        profiles: vec![profile],
        ..AppConfig::default()
    };
    config.validate().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("type = \"telnet\""));
    assert!(!encoded.contains("username"));
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("credential"));
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.profiles[0].id, profile_id);
    assert_eq!(decoded.profiles[0].connection, Connection::Telnet(telnet));
    assert!(!decoded.uses_credential(CredentialRef::new()));

    for field in ["username", "password", "credential"] {
        assert!(
            toml::from_str::<TelnetConfig>(&format!(
                "host = 'localhost'\n{field} = 'do-not-store-this'"
            ))
            .is_err()
        );
    }
}

#[test]
fn application_lock_defaults_off_for_existing_configuration() {
    let config: AppConfig = toml::from_str("schema_version = 1").unwrap();
    config.validate().unwrap();
    assert!(config.app_lock.password_hash.is_none());
    assert_eq!(config.app_lock.idle_minutes, 0);
}

#[test]
fn application_lock_validates_persisted_hash_and_idle_timeout_without_hashing() {
    let hash = "$argon2id$v=19$m=19456,t=2,p=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let mut config = AppConfig {
        app_lock: AppLockConfig {
            password_hash: Some(hash.into()),
            idle_minutes: 15,
        },
        ..AppConfig::default()
    };
    config.validate().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.app_lock.password_hash.as_deref(), Some(hash));
    assert_eq!(decoded.app_lock.idle_minutes, 15);
    for idle_minutes in [0, 1440] {
        config.app_lock.idle_minutes = idle_minutes;
        config.validate().unwrap();
    }
    config.app_lock.idle_minutes = 1441;
    assert!(config.validate().is_err());
    config.app_lock.idle_minutes = 15;
    config.app_lock.password_hash = None;
    assert!(config.validate().is_err());
    config.app_lock.idle_minutes = 0;
    config.validate().unwrap();
    config.app_lock.password_hash = Some("invalid-hash-sentinel".into());
    let error = config.validate().unwrap_err();
    assert!(!error.to_string().contains("invalid-hash-sentinel"));
    assert!(!format!("{error:?}").contains("invalid-hash-sentinel"));
}

#[test]
fn rdp_profiles_round_trip_with_vault_references_and_normalized_certificate_pins() {
    let reference = CredentialRef(Uuid::from_u128(70));
    let settings = RdpConfig {
        username: "operator".into(),
        domain: Some("WORK".into()),
        credential: Some(reference),
        certificate_sha256: Some("ab".repeat(32)),
        ..RdpConfig::default()
    };
    let profile = Profile::rdp("Desktop", settings.clone());
    let profile_id = profile.id;
    let config = AppConfig {
        profiles: vec![profile],
        ..AppConfig::default()
    };
    config.validate().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("type = \"rdp\""));
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("secret"));
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.profiles[0].id, profile_id);
    assert_eq!(decoded.profiles[0].connection, Connection::Rdp(settings));
    assert!(decoded.uses_credential(reference));
    assert!(!decoded.uses_credential(CredentialRef(Uuid::from_u128(71))));

    let uppercase_pin = "AB".repeat(32);
    let parsed: RdpConfig = toml::from_str(&format!(
        "username = 'operator'\ncertificate_sha256 = '{uppercase_pin}'\n"
    ))
    .unwrap();
    assert_eq!(parsed.certificate_sha256, Some("ab".repeat(32)));
    let serialized = toml::to_string(&RdpConfig {
        certificate_sha256: Some(uppercase_pin),
        ..parsed
    })
    .unwrap();
    assert!(serialized.contains(&"ab".repeat(32)));
}

#[test]
fn rdp_plaintext_and_certificate_bypass_options_are_rejected_without_echoing_secrets() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    for unsupported in ["password = 'rdp-secret-sentinel'", "skip_verify = true"] {
        let source = format!(
            r#"schema_version = 1
[[profiles]]
id = "00000000-0000-0000-0000-000000000001"
name = "Desktop"
[profiles.connection]
type = "rdp"
[profiles.connection.options]
username = "operator"
{unsupported}
"#
        );
        write_private(&path, &source);
        let error = AppConfig::load(&path).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("rdp-secret-sentinel"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
}

#[test]
fn legacy_configuration_without_language_uses_system_default() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    let source = "schema_version = 1\n[settings]\nfont_size = 15.0\n";
    write_private(&path, source);
    let config = AppConfig::load(&path).unwrap();
    assert_eq!(config.settings.language, crate::i18n::Language::System);
    assert_eq!(config.settings.font_size, 15.0);
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
}

#[test]
fn loading_language_does_not_rename_existing_profiles() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    let mut config = AppConfig::default();
    config.profiles[0].name = "原有名称 My host".into();
    config.settings.language = crate::i18n::Language::English;
    config.save(&path).unwrap();
    let loaded = AppConfig::load(path).unwrap();
    assert_eq!(loaded.profiles[0].name, "原有名称 My host");
    assert_eq!(loaded.profiles[0].id, config.profiles[0].id);
}

// Public-only fixtures: ssh-key upstream examples and an encoded Edwards base point.
const ED25519_A: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
const ED25519_B: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFhmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZm";
const RSA_PUBLIC: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCmjkeMm8k3JkNrf16eb5pG4bc77B6Mt3VN4saltsRV8vASpyWa/PlBgdaeldOaNJ5NK0gqU3KyiUNzHbdcc8572e7IUBDJS/rlaWARiSL4aos2VbNX0k56Z5zYp9m/bq5m9/mlb+PQkNBjIhimgpYNiq2TwBiYeA6tLb79cPtHA0cX5BLk/a5oUpLsiR4kI/f+Q98vVDKasKXXVh5YLkLobrruDB6er2A9fOcIUF0O4JCRLh/Dc161gE3fQrYTMQenbppZzfxrZfQ8YwLPvKjnqm+XRX+pbTtaJuj0EgTSzUK+EZxoSw8CNwiZpxrjwecTMVQ8w/srQmh4ABGuTqk0wP8HcI7hg+fpBv7kiejh5X/Oehxt+Puu85u9GVXb1a0av/vhJvUCBcuISvCA/z1wVJ0xdLhb1/ZiTDdTzyNbZQ0OQijzK+e1SlkNhp+3eGVZu3pNZvnTppwIXv3wg6kV1HodkWGgh1ayY7Buc52Z8okDYqvJat5CzOj5OaQNr/k=";

#[test]
fn round_trip_preserves_ids_and_never_serializes_secrets() {
    let mut config = AppConfig::default();
    let group = Group::new("生产环境");
    config.profiles[0].group = Some(group.id);
    config.groups.push(group);
    config.profiles.push(Profile::ssh(
        "same name",
        SshConfig {
            host: "example.invalid".into(),
            username: "test".into(),
            auth: AuthMethod::Password {
                credential: Some(CredentialRef::new()),
            },
            ..SshConfig::default()
        },
    ));
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    config.save(&path).unwrap();
    let loaded = AppConfig::load(&path).unwrap();
    assert_eq!(loaded.profiles[0].id, config.profiles[0].id);
    assert_eq!(loaded.profiles[1].id, config.profiles[1].id);
    assert_ne!(loaded.profiles[0].id, loaded.profiles[1].id);
    assert_eq!(loaded.profiles[0].group, config.profiles[0].group);
    assert_eq!(loaded.settings.history_lines, 50_000);
    let source = std::fs::read_to_string(path).unwrap();
    assert!(source.contains("credential ="));
    assert!(!source.contains("secret ="));
}

#[test]
fn future_or_malformed_files_are_not_overwritten() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    for source in [
        "schema_version = 999",
        "not valid toml = [",
        "schema_version = 1\npassword = 'must-not-be-accepted'",
    ] {
        write_private(&path, source);
        assert!(AppConfig::load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
}

#[test]
fn parse_errors_never_retain_sensitive_source() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    for source in [
        "schema_version = 1\npassword = 'do-not-echo-secret'",
        "schema_version = 'do-not-echo-secret'\n",
        "password = ['do-not-echo-secret'",
    ] {
        write_private(&path, source);
        let error = AppConfig::load(&path).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("do-not-echo-secret"));
        assert!(!error.was_committed());
    }
    assert!(ConfigError::CommitUncertain(std::io::Error::other("test")).was_committed());
}

#[test]
fn stale_host_management_confirmation_cannot_remove_new_trust() {
    let directory = private_directory();
    let path = directory.path().join("known-hosts.toml");
    KnownHosts::trust_and_save(&path, "example.invalid", 22, "ssh-ed25519", ED25519_A).unwrap();
    let original = KnownHosts::load(&path).unwrap().entries()[0].keys.clone();
    let mut changed = KnownHosts::load(&path).unwrap();
    changed
        .trust_explicit("example.invalid", 22, "ssh-ed25519", ED25519_B)
        .unwrap();
    changed.save(&path).unwrap();
    assert!(
        KnownHosts::remove_if_unchanged_and_save(&path, "example.invalid", 22, &original).is_err()
    );
    let current = KnownHosts::load(&path).unwrap().entries()[0].keys.clone();
    assert_eq!(current[0].public_key, ED25519_B);
    assert!(
        KnownHosts::remove_if_unchanged_and_save(&path, "example.invalid", 22, &current).unwrap()
    );
}

#[test]
fn migration_backs_up_valid_legacy_config() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    let old = "profiles = []\ngroups = []\n[settings]\nfont_size = 17.0\n";
    write_private(&path, old);
    let loaded = AppConfig::load(&path).unwrap();
    assert_eq!(loaded.schema_version, SCHEMA_VERSION);
    assert_eq!(loaded.settings.font_size, 17.);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("config.toml.v0.bak")).unwrap(),
        old
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("schema_version = 1")
    );
}

#[test]
fn invalid_saves_leave_previous_configuration_intact() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    let mut config = AppConfig::default();
    config.save(&path).unwrap();
    let previous = std::fs::read(&path).unwrap();
    config.settings.font_size = f32::NAN;
    assert!(config.save(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), previous);
}

#[test]
fn validation_rejects_duplicate_ids_dangling_groups_and_bad_budgets() {
    let mut config = AppConfig::default();
    config.profiles.push(config.profiles[0].clone());
    assert!(config.validate().is_err());
    config.profiles.pop();
    config.profiles[0].group = Some(Uuid::new_v4());
    assert!(config.validate().is_err());
    config.profiles[0].group = None;
    config.settings.global_history_bytes = 100;
    assert!(config.validate().is_err());
}

#[test]
fn host_trust_checks_target_port_algorithm_and_full_key() {
    let mut hosts = KnownHosts::default();
    assert_eq!(
        hosts.check("EXAMPLE.invalid.", 22, "ssh-ed25519", ED25519_A),
        HostKeyStatus::Unknown
    );
    hosts
        .trust("EXAMPLE.invalid.", 22, "ssh-ed25519", ED25519_A)
        .unwrap();
    assert_eq!(
        hosts.check("example.invalid", 22, "ssh-ed25519", ED25519_A),
        HostKeyStatus::Trusted
    );
    assert_eq!(
        hosts.check("example.invalid", 2222, "ssh-ed25519", ED25519_A),
        HostKeyStatus::Unknown
    );
    assert!(matches!(
        hosts.check("example.invalid", 22, "ssh-ed25519", ED25519_B),
        HostKeyStatus::Changed { .. }
    ));
    assert!(
        hosts
            .trust("example.invalid", 22, "ssh-ed25519", ED25519_B)
            .is_err()
    );
    assert!(
        hosts
            .trust("example.invalid", 22, "ssh-rsa", RSA_PUBLIC)
            .is_err()
    );
    hosts
        .trust_explicit("example.invalid", 22, "ssh-rsa", RSA_PUBLIC)
        .unwrap();
    assert_eq!(
        hosts.check("example.invalid", 22, "ssh-ed25519", ED25519_A),
        HostKeyStatus::Trusted
    );
    assert_eq!(
        hosts.check("example.invalid", 22, "ssh-rsa", RSA_PUBLIC),
        HostKeyStatus::Trusted
    );
}

#[test]
fn corrupt_host_file_does_not_become_empty_trust() {
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    write_private(&path, "broken trust file");
    assert!(KnownHosts::load(&path).is_err());
    assert!(
        KnownHosts::load(directory.path().join("missing"))
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn malformed_host_keys_and_algorithm_mismatches_preserve_original_file() {
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let mismatched_header = ED25519_A.replacen("ssh-ed25519", "ssh-rsa", 1);
    for (algorithm, key) in [
        ("ssh-ed25519", "ssh-ed25519 not-base64!"),
        ("ssh-ed25519", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAAQ=="),
        ("ssh-rsa", ED25519_A),
        ("rsa-sha2-512", RSA_PUBLIC),
        ("ssh-ed25519", mismatched_header.as_str()),
    ] {
        let source = format!(
            "version = 1\n[[hosts]]\nhost = \"example.invalid\"\nport = 22\n[[hosts.keys]]\nalgorithm = {algorithm:?}\npublic_key = {key:?}\n"
        );
        write_private(&path, &source);
        assert!(KnownHosts::load(&path).is_err());
        assert!(
            KnownHosts::trust_and_save(&path, "another.invalid", 22, "ssh-ed25519", ED25519_A)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let mut empty = KnownHosts::default();
        assert!(empty.trust("example.invalid", 22, algorithm, key).is_err());
        assert!(empty.entries().is_empty());
    }
}

#[test]
fn host_records_round_trip_and_normalize_ipv6() {
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let mut hosts = KnownHosts::default();
    hosts.trust("[::1]", 22, "ssh-ed25519", ED25519_A).unwrap();
    hosts.save(&path).unwrap();
    let mut loaded = KnownHosts::load(&path).unwrap();
    assert_eq!(
        loaded.check("0:0:0:0:0:0:0:1", 22, "ssh-ed25519", ED25519_A),
        HostKeyStatus::Trusted
    );
    assert!(loaded.remove("::1", 22).unwrap());
}

#[cfg(unix)]
#[test]
fn sensitive_files_are_private_and_unsafe_links_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    AppConfig::default().save(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let link = directory.path().join("link.toml");
    symlink(&path, &link).unwrap();
    assert!(AppConfig::load(&link).is_err());
    assert!(AppConfig::default().save(&link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(AppConfig::load(&path).is_err());
}

#[test]
fn credential_reference_validation_never_contacts_backend() {
    let store = SystemCredentialStore::new();
    assert!(matches!(
        store.get(CredentialRef(Uuid::nil())),
        Err(credentials::CredentialError::InvalidReference)
    ));
    let secret = Secret::new("synthetic-only".into());
    assert!(store.set(CredentialRef(Uuid::nil()), &secret).is_err());
}

#[test]
fn instance_lock_excludes_second_writer_and_releases_on_drop() {
    let directory = private_directory();
    let first = ConfigLock::acquire(directory.path()).unwrap();
    assert!(matches!(
        ConfigLock::acquire(directory.path()),
        Err(ConfigError::AlreadyRunning)
    ));
    drop(first);
    assert!(ConfigLock::acquire(directory.path()).is_ok());
}

#[test]
fn concurrent_host_confirmations_do_not_overwrite_each_other() {
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    std::thread::scope(|scope| {
        for index in 0..8 {
            let path = &path;
            scope.spawn(move || {
                KnownHosts::trust_and_save(
                    path,
                    &format!("host-{index}.invalid"),
                    22,
                    "ssh-ed25519",
                    ED25519_A,
                )
                .unwrap();
            });
        }
    });
    assert_eq!(KnownHosts::load(&path).unwrap().entries().len(), 8);
    assert!(
        KnownHosts::trust_and_save(&path, "host-1.invalid", 22, "ssh-ed25519", ED25519_B).is_err()
    );
    assert!(KnownHosts::remove_and_save(&path, "host-1.invalid", 22).unwrap());
    assert_eq!(KnownHosts::load(&path).unwrap().entries().len(), 7);
}

#[test]
fn fingerprint_uses_decoded_ssh_key_and_rejects_invalid_encoding() {
    let key = HostKey {
        algorithm: "ssh-ed25519".into(),
        public_key: ED25519_A.into(),
    };
    assert!(key.fingerprint().unwrap().starts_with("SHA256:"));
    let invalid = HostKey {
        public_key: "ssh-ed25519 not-base64!".into(),
        ..key.clone()
    };
    assert!(invalid.fingerprint().is_err());
    let mismatched = HostKey {
        algorithm: "ssh-rsa".into(),
        ..key
    };
    assert!(mismatched.fingerprint().is_err());
}

#[test]
fn unavailable_keyring_is_an_error_without_plaintext_fallback() {
    let directory = private_directory();
    // libtest names omit the crate prefix, while module_path! includes it.
    let (_, module) = module_path!().split_once("::").unwrap();
    let child_test = format!("{module}::unavailable_keyring_child");
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "--format=pretty", "--color=never"])
        .arg(&child_test)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/no-session-bus", directory.path().display()),
        )
        .env("RUSTTERM_TEST_NO_SECRET_SERVICE", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "credential child failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    let passed = format!("test {child_test} ... ok");
    assert!(
        output.lines().any(|line| line == passed),
        "credential child did not execute the requested test: {output}"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "isolated subprocess with deliberately unavailable D-Bus; invoked by the parent test"]
fn unavailable_keyring_child() {
    assert_eq!(
        std::env::var("RUSTTERM_TEST_NO_SECRET_SERVICE").as_deref(),
        Ok("1")
    );
    let store = SystemCredentialStore::new();
    let reference = CredentialRef::new();
    let secret = Secret::new("synthetic-secret-service-failure-fixture".into());
    assert!(matches!(
        store.set(reference, &secret),
        Err(credentials::CredentialError::Unavailable)
    ));
    assert!(matches!(
        store.get(reference),
        Err(credentials::CredentialError::Unavailable)
    ));
}

fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    directory
}
fn write_private(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn redis_defaults_and_legacy_profiles_remain_compatible() {
    let redis: RedisConfig = toml::from_str("").unwrap();
    assert_eq!(redis, RedisConfig::default());
    assert_eq!(
        (
            redis.host.as_str(),
            redis.port,
            redis.database,
            redis.timeout_secs
        ),
        ("127.0.0.1", 6379, 0, 10)
    );
    assert!(!redis.tls);
    assert!(redis.username.is_none());
    assert!(redis.credential.is_none());
    redis.validate().unwrap();

    let legacy = r#"
schema_version = 1
[[profiles]]
id = "00000000-0000-0000-0000-000000000001"
name = "Existing shell"
[profiles.connection]
type = "local"
[profiles.connection.options]
shell = "/bin/sh"
args = ["-l"]
"#;
    let loaded: AppConfig = toml::from_str(legacy).unwrap();
    loaded.validate().unwrap();
    assert_eq!(loaded.profiles.len(), 1);
    assert_eq!(loaded.profiles[0].name, "Existing shell");
    assert!(
        matches!(&loaded.profiles[0].connection, Connection::Local(local) if local.args == ["-l"])
    );
}

#[test]
fn redis_profile_round_trip_preserves_credential_references_without_password_fields() {
    let reference = CredentialRef(Uuid::from_u128(13));
    let redis = RedisConfig {
        host: "cache.example.invalid".into(),
        username: Some("reader".into()),
        credential: Some(reference),
        database: 2,
        tls: true,
        ..RedisConfig::default()
    };
    let profile = Profile::redis("Cache", redis.clone());
    let profile_id = profile.id;
    let config = AppConfig {
        profiles: vec![profile],
        ..AppConfig::default()
    };
    config.validate().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("type = \"redis\""));
    assert!(encoded.contains("credential ="));
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("secret"));
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.profiles[0].id, profile_id);
    assert_eq!(decoded.profiles[0].connection, Connection::Redis(redis));
    assert!(decoded.uses_credential(reference));
    assert!(!decoded.uses_credential(CredentialRef(Uuid::from_u128(14))));
}

#[test]
fn redis_plaintext_and_insecure_tls_options_are_rejected_without_echoing_the_source() {
    let directory = private_directory();
    let path = directory.path().join("config.toml");
    for unsupported in ["password = 'redis-secret-sentinel'", "tls_insecure = true"] {
        let source = format!(
            r#"schema_version = 1
[[profiles]]
id = "00000000-0000-0000-0000-000000000001"
name = "Cache"
[profiles.connection]
type = "redis"
[profiles.connection.options]
host = "127.0.0.1"
{unsupported}
"#
        );
        write_private(&path, &source);
        let error = AppConfig::load(&path).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("redis-secret-sentinel"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
}

#[test]
fn redis_tunnels_reference_only_existing_ssh_profiles_and_preserve_ids() {
    let ssh = Profile::ssh(
        "Gateway",
        SshConfig {
            host: "gateway.example.invalid".into(),
            username: "operator".into(),
            ..SshConfig::default()
        },
    );
    let local = Profile::local("Shell", LocalConfig::default());
    let direct = Profile::redis("Direct", RedisConfig::default());
    let tunneled = Profile::redis(
        "Private cache",
        RedisConfig {
            ssh_profile: Some(ssh.id),
            ..RedisConfig::default()
        },
    );
    let mut config = AppConfig {
        profiles: vec![ssh.clone(), local.clone(), direct.clone(), tunneled.clone()],
        ..AppConfig::default()
    };
    config.validate().unwrap();
    assert!(config.uses_ssh_profile(ssh.id));
    assert!(!config.uses_ssh_profile(local.id));
    let encoded = toml::to_string(&config).unwrap();
    let decoded: AppConfig = toml::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert!(
        matches!(&decoded.profiles[3].connection, Connection::Redis(redis) if redis.ssh_profile == Some(ssh.id))
    );
    for invalid in [
        Uuid::nil(),
        Uuid::from_u128(999),
        local.id,
        direct.id,
        tunneled.id,
    ] {
        let Connection::Redis(redis) = &mut config.profiles[3].connection else {
            unreachable!()
        };
        redis.ssh_profile = Some(invalid);
        assert!(config.validate().is_err());
    }
    config.profiles.pop();
    assert!(!config.uses_ssh_profile(ssh.id));
}

#[test]
fn redis_without_a_tunnel_field_defaults_to_direct_connection() {
    let existing: RedisConfig =
        toml::from_str("host = '127.0.0.1'\nport = 6379\ndatabase = 2\ntls = true\n").unwrap();
    assert_eq!(existing.ssh_profile, None);
    assert_eq!(existing.database, 2);
    assert!(existing.tls);
    existing.validate().unwrap();
}

#[test]
fn ssh_tab_group_collapse_state_round_trips_and_older_workspaces_default_expanded() {
    let old: SavedWorkspace = toml::from_str("active = 0\nfocused = 0").unwrap();
    assert!(old.collapsed_ssh_tab_groups.is_empty());
    let profile = Uuid::new_v4();
    let mut snapshot = WorkspaceSnapshot {
        layout: SavedWorkspace {
            tabs: vec![
                SavedTab {
                    title: "server".into(),
                    layout: SavedLayout::Pane { index: 0 },
                },
                SavedTab {
                    title: "server:2".into(),
                    layout: SavedLayout::Pane { index: 1 },
                },
            ],
            collapsed_ssh_tab_groups: vec![profile],
            ..Default::default()
        },
        panes: vec![
            PanePosition {
                profile,
                ..Default::default()
            };
            2
        ],
    };
    snapshot.validate().unwrap();
    let restored: WorkspaceSnapshot = toml::from_str(&toml::to_string(&snapshot).unwrap()).unwrap();
    assert_eq!(restored, snapshot);
    snapshot.layout.collapsed_ssh_tab_groups.push(profile);
    assert!(snapshot.validate().is_err());
    snapshot.layout.collapsed_ssh_tab_groups = vec![Uuid::new_v4()];
    assert!(snapshot.validate().is_err());
}

#[test]
fn workspace_preserves_ssh_session_numbers_and_accepts_older_positions() {
    let profile = Profile::ssh("server", SshConfig::default());
    let old: PanePosition = toml::from_str(&format!("profile = '{}'", profile.id)).unwrap();
    assert_eq!(old.ssh_session, None);
    let mut config = AppConfig {
        profiles: vec![profile.clone()],
        workspace: WorkspaceSnapshot {
            layout: SavedWorkspace {
                tabs: vec![SavedTab {
                    title: profile.name.clone(),
                    layout: SavedLayout::Pane { index: 0 },
                }],
                ..Default::default()
            },
            panes: vec![PanePosition {
                profile: profile.id,
                ssh_session: Some(SshSessionPosition { number: 3, last: 5 }),
                ..Default::default()
            }],
        },
        ..Default::default()
    };
    // Only the workspace is relevant here; the fixture has no remote SSH target.
    config.workspace.validate().unwrap();
    let decoded: AppConfig = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
    assert_eq!(decoded.workspace, config.workspace);
    assert_eq!(decoded.profiles[0].name, "server");
    for session in [
        SshSessionPosition { number: 0, last: 5 },
        SshSessionPosition { number: 3, last: 2 },
    ] {
        config.workspace.panes[0].ssh_session = Some(session);
        assert!(config.workspace.validate().is_err());
    }
}

#[test]
fn workspace_and_commands_round_trip_and_reject_invalid_layout_references() {
    let profile = Profile::local("Local", LocalConfig::default());
    let mut config = AppConfig {
        workspace: WorkspaceSnapshot {
            layout: SavedWorkspace {
                tabs: vec![SavedTab {
                    title: "Work".into(),
                    layout: SavedLayout::Pane { index: 0 },
                }],
                ..Default::default()
            },
            panes: vec![PanePosition {
                profile: profile.id,
                directory: Some("/tmp/project with spaces".into()),
                ..Default::default()
            }],
        },
        quick_commands: vec![QuickCommand {
            id: Uuid::new_v4(),
            name: "Inspect".into(),
            command: "pwd\nprintf '%s\\n' \"$PATH\"".into(),
        }],
        profiles: vec![profile],
        ..Default::default()
    };
    config.validate().unwrap();
    let decoded: AppConfig = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
    assert_eq!(decoded.workspace, config.workspace);
    assert_eq!(decoded.quick_commands, config.quick_commands);
    config.workspace.layout.tabs[0].layout = SavedLayout::Split {
        vertical: false,
        ratio: 0.5,
        first: Box::new(SavedLayout::Pane { index: 0 }),
        second: Box::new(SavedLayout::Pane { index: 0 }),
    };
    assert!(config.validate().is_err());
    config.workspace.layout.tabs[0].layout = SavedLayout::Pane { index: 1 };
    assert!(config.validate().is_err());
}

#[test]
fn saved_commands_have_an_aggregate_storage_budget() {
    let mut config = AppConfig {
        quick_commands: (0..16)
            .map(|_| QuickCommand {
                id: Uuid::new_v4(),
                name: "Script".into(),
                command: "x".repeat(65_536 - 6),
            })
            .collect(),
        ..Default::default()
    };
    config.validate().unwrap();
    config.quick_commands[0].command.push('x');
    assert!(config.validate().is_err());
}
