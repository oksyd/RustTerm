use crate::i18n::messages as i18n_key;

use super::credentials::{CredentialRef, validate_reference};
use super::{Result, invalid, known_hosts, validate_path, validate_text};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Remote desktop settings. Passwords are stored separately in the OS vault.
/// A certificate pin is created only after explicitly trusting a server certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: Option<String>,
    pub remote_app: Option<RdpRemoteApp>,
    pub credential: Option<CredentialRef>,
    pub width: u16,
    pub height: u16,
    pub dynamic_resolution: bool,
    pub color_depth: u16,
    /// None follows the local display scale when the connection is opened.
    pub desktop_scale: Option<u32>,
    pub max_fps: u16,
    pub allow_lossy_bitmaps: bool,
    pub compression: bool,
    pub clipboard: bool,
    pub clipboard_files: bool,
    pub audio_output: bool,
    pub audio_input: bool,
    pub monitor_count: u8,
    pub shared_folders: Vec<RdpSharedFolder>,
    pub timeout_secs: u64,
    /// TCP idle/probe interval. Zero disables network keepalive; this does not
    /// synthesize user input or inhibit the remote desktop's lock/sleep policy.
    pub keepalive_secs: u64,
    /// Periodically move and restore the remote pointer to prevent desktop idle.
    /// Separate from TCP keepalive because this synthesizes remote input.
    pub prevent_idle: bool,
    #[serde(
        deserialize_with = "deserialize_rdp_certificate_sha256",
        serialize_with = "serialize_rdp_certificate_sha256"
    )]
    pub certificate_sha256: Option<String>,
}

impl Default for RdpConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 3389,
            username: String::new(),
            domain: None,
            remote_app: None,
            credential: None,
            width: 1280,
            height: 720,
            dynamic_resolution: true,
            color_depth: 32,
            desktop_scale: None,
            max_fps: 60,
            allow_lossy_bitmaps: false,
            compression: true,
            clipboard: true,
            clipboard_files: cfg!(target_os = "linux"),
            audio_output: cfg!(target_os = "linux"),
            audio_input: false,
            monitor_count: 1,
            shared_folders: Vec::new(),
            timeout_secs: 15,
            keepalive_secs: 30,
            prevent_idle: false,
            certificate_sha256: None,
        }
    }
}

/// RemoteApp launch parameters. The server must publish/allow the application.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RdpRemoteApp {
    pub program: String,
    pub arguments: String,
    pub working_directory: String,
}

impl RdpRemoteApp {
    fn validate(&self) -> Result<()> {
        for (text, max, field) in [
            (&self.program, 260, i18n_key::RDP_REMOTE_APP_PROGRAM),
            (&self.arguments, 8000, i18n_key::RDP_REMOTE_APP_ARGUMENTS),
            (
                &self.working_directory,
                260,
                i18n_key::RDP_REMOTE_APP_DIRECTORY,
            ),
        ] {
            if text.encode_utf16().count() > max
                || text.chars().any(char::is_control)
                || (field == i18n_key::RDP_REMOTE_APP_PROGRAM && text.trim().is_empty())
            {
                return Err(invalid(i18n_key::RDP_REMOTE_APP_INVALID_PARAMETERS).at_field(field));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdpQuality {
    Clear,
    Smooth,
    Bandwidth,
    Custom,
}

impl RdpConfig {
    pub fn quality(&self) -> RdpQuality {
        match (
            self.color_depth,
            self.max_fps,
            self.allow_lossy_bitmaps,
            self.compression,
        ) {
            (32, 60, false, true) => RdpQuality::Clear,
            (32, 60, true, true) => RdpQuality::Smooth,
            (32, 30, true, true) => RdpQuality::Bandwidth,
            _ => RdpQuality::Custom,
        }
    }

    pub fn apply_quality(&mut self, quality: RdpQuality) {
        let (max_fps, allow_lossy_bitmaps) = match quality {
            RdpQuality::Clear => (60, false),
            RdpQuality::Smooth => (60, true),
            RdpQuality::Bandwidth => (30, true),
            RdpQuality::Custom => return,
        };
        self.color_depth = 32;
        self.max_fps = max_fps;
        self.allow_lossy_bitmaps = allow_lossy_bitmaps;
        self.compression = true;
    }

    /// A stored multi-monitor layout has fixed dimensions, including older profiles
    /// that also enabled automatic single-monitor resizing.
    pub fn uses_dynamic_resolution(&self) -> bool {
        self.remote_app.is_none() && self.dynamic_resolution && self.monitor_count == 1
    }

    pub fn same_server(&self, other: &Self) -> bool {
        self.host == other.host && self.port == other.port
    }

    /// Presentation preferences and rotating vault references do not change identity.
    pub fn same_auth_identity(&self, other: &Self) -> bool {
        self.same_server(other) && self.username == other.username && self.domain == other.domain
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(app) = &self.remote_app {
            app.validate()?;
        }
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        validate_text(&self.username, 255, i18n_key::RDP_RDP_USERNAME)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_USERNAME))?;
        if let Some(domain) = &self.domain {
            validate_text(domain, 255, i18n_key::RDP_RDP_DOMAIN)
                .map_err(|error| error.at_field(i18n_key::RDP_DOMAIN_OPTIONAL))?;
        }
        if self.port == 0 {
            return Err(invalid(i18n_key::RDP_RDP_PORT_MUST_BE_BETWEEN_1_AND_65535)
                .at_field(i18n_key::WORKSPACE_PORT));
        }
        if !(1..=600).contains(&self.timeout_secs) {
            return Err(invalid(
                i18n_key::RDP_RDP_CONNECTION_TIMEOUT_MUST_BE_BETWEEN_1_AND_600_SECONDS,
            )
            .at_field(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS));
        }
        if self.keepalive_secs > 3600 {
            return Err(invalid(i18n_key::RDP_KEEPALIVE_INVALID)
                .at_field(i18n_key::RDP_NETWORK_KEEPALIVE_SECONDS));
        }
        if !matches!(self.color_depth, 16 | 32) {
            return Err(invalid(i18n_key::RDP_RDP_COLOR_DEPTH_MUST_BE_16_OR_32_BITS)
                .at_field(i18n_key::RDP_COLOR_DEPTH));
        }
        if self
            .desktop_scale
            .is_some_and(|scale| !(100..=500).contains(&scale))
        {
            return Err(
                invalid(i18n_key::RDP_RDP_DESKTOP_SCALING_MUST_BE_BETWEEN_100_AND_500)
                    .at_field(i18n_key::RDP_REMOTE_DESKTOP_SCALING),
            );
        }
        if !matches!(self.max_fps, 30 | 60) {
            return Err(
                invalid(i18n_key::RDP_RDP_LOCAL_REFRESH_LIMIT_MUST_BE_30_OR_60_FPS)
                    .at_field(i18n_key::RDP_LOCAL_REFRESH_LIMIT),
            );
        }
        let monitor_count = if self.remote_app.is_some() {
            1
        } else {
            self.monitor_count
        };
        if !(320..=3840).contains(&self.width)
            || !(200..=2160).contains(&self.height)
            || !(1..=4).contains(&self.monitor_count)
            || (monitor_count > 1 && !self.width.is_multiple_of(2))
            || u32::from(self.width) * u32::from(monitor_count) > 8192
            || u32::from(self.width) * u32::from(self.height) * u32::from(monitor_count)
                > 33_554_432
        {
            return Err(
                invalid(i18n_key::RDP_USE_1_4_RDP_MONITORS_EACH_320_3840_BY_200).at_field(
                    if !(320..=3840).contains(&self.width) {
                        i18n_key::RDP_DESKTOP_WIDTH_PIXELS
                    } else if !(200..=2160).contains(&self.height) {
                        i18n_key::RDP_DESKTOP_HEIGHT_PIXELS
                    } else if !(1..=4).contains(&self.monitor_count) {
                        i18n_key::RDP_REMOTE_MONITORS_1_4
                    } else {
                        i18n_key::RDP_DESKTOP_WIDTH_PIXELS
                    },
                ),
            );
        }
        if self
            .certificate_sha256
            .as_ref()
            .is_some_and(|pin| pin.len() != 64 || !pin.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(invalid(i18n_key::RDP_RDP_CERTIFICATE_FINGERPRINT_MUST_CONTAIN_EXACTLY_64_HEXADECIMAL_CHARACTERS));
        }
        if self.shared_folders.len() > 16 {
            return Err(invalid(i18n_key::RDP_SHARE_AT_MOST_16_LOCAL_DIRECTORIES));
        }
        for (index, folder) in self.shared_folders.iter().enumerate() {
            if folder.name.is_empty()
                || folder.name.len() > 7
                || !folder
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || self.shared_folders[..index]
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(&folder.name))
                || !folder.path.is_absolute()
            {
                return Err(invalid(
                    i18n_key::RDP_SHARED_DIRECTORY_NAMES_MUST_BE_DISTINCT_1_7_ASCII_LETTERS,
                ));
            }
            validate_path(&folder.path, i18n_key::RDP_SHARED_DIRECTORY)?;
        }
        validate_reference(&self.credential)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RdpSharedFolder {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_read_only_share")]
    pub read_only: bool,
}
fn default_read_only_share() -> bool {
    true
}

fn deserialize_rdp_certificate_sha256<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.map(|value| value.to_ascii_lowercase()))
}

fn serialize_rdp_certificate_sha256<S>(
    value: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value
        .as_ref()
        .map(|value| value.to_ascii_lowercase())
        .serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::{RdpConfig, RdpQuality, RdpRemoteApp};
    use crate::config::CredentialRef;
    use crate::i18n::messages as i18n_key;
    use uuid::Uuid;

    #[test]
    fn older_profiles_keep_clear_defaults_and_round_trip_custom_preferences() {
        let mut config: RdpConfig = toml::from_str("username = 'operator'").unwrap();
        config.validate().unwrap();
        assert_eq!(config.quality(), RdpQuality::Clear);
        assert_eq!(config.desktop_scale, None);
        assert_eq!(config.keepalive_secs, 30);
        assert!(!config.prevent_idle);
        assert!(config.uses_dynamic_resolution());
        config.color_depth = 16;
        config.desktop_scale = Some(175);
        config.max_fps = 30;
        config.allow_lossy_bitmaps = true;
        config.compression = false;
        config.keepalive_secs = 45;
        config.prevent_idle = true;
        config.validate().unwrap();
        assert_eq!(config.quality(), RdpQuality::Custom);
        let encoded = toml::to_string(&config).unwrap();
        let decoded: RdpConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn keepalive_can_be_disabled_and_invalid_intervals_identify_the_field() {
        let valid = RdpConfig {
            username: "operator".into(),
            ..RdpConfig::default()
        };
        for seconds in [0, 1, 30, 3600] {
            let config = RdpConfig {
                keepalive_secs: seconds,
                ..valid.clone()
            };
            config.validate().unwrap();
            let decoded: RdpConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(decoded.keepalive_secs, seconds);
            assert!(valid.same_auth_identity(&decoded));
        }
        for seconds in [3601, u64::MAX] {
            let error = RdpConfig {
                keepalive_secs: seconds,
                ..valid.clone()
            }
            .validate()
            .unwrap_err();
            assert!(matches!(
                error,
                crate::config::ConfigError::FieldValidation {
                    field: i18n_key::RDP_NETWORK_KEEPALIVE_SECONDS,
                    ..
                }
            ));
        }
    }

    #[test]
    fn quality_presets_derive_from_preferences_without_changing_display_layout() {
        let mut config = RdpConfig {
            username: "operator".into(),
            width: 1920,
            height: 1080,
            desktop_scale: Some(150),
            monitor_count: 2,
            ..RdpConfig::default()
        };
        config.validate().unwrap();
        assert!(!config.uses_dynamic_resolution());
        config.apply_quality(RdpQuality::Bandwidth);
        assert_eq!(config.quality(), RdpQuality::Bandwidth);
        assert_eq!(config.color_depth, 32);
        assert_eq!(config.max_fps, 30);
        assert!(config.allow_lossy_bitmaps);
        assert!(config.compression);
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.desktop_scale, Some(150));
        assert_eq!(config.monitor_count, 2);
        config.apply_quality(RdpQuality::Smooth);
        assert_eq!(config.quality(), RdpQuality::Smooth);
        assert_eq!(config.max_fps, 60);
        assert!(config.allow_lossy_bitmaps);
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.desktop_scale, Some(150));
        assert_eq!(config.monitor_count, 2);
        config.color_depth = 16;
        assert_eq!(config.quality(), RdpQuality::Custom);
        let custom = config.clone();
        config.apply_quality(RdpQuality::Custom);
        assert_eq!(config, custom);
        config.apply_quality(RdpQuality::Clear);
        assert_eq!(config.quality(), RdpQuality::Clear);
        assert!(!config.allow_lossy_bitmaps);
        assert_eq!(config.max_fps, 60);
        config.compression = false;
        assert_eq!(config.quality(), RdpQuality::Custom);
    }

    #[test]
    fn invalid_display_preferences_identify_the_corresponding_form_field() {
        let valid = RdpConfig {
            username: "operator".into(),
            ..RdpConfig::default()
        };
        for color_depth in [0, 8, 24, 48] {
            let error = RdpConfig {
                color_depth,
                ..valid.clone()
            }
            .validate()
            .unwrap_err();
            assert_eq!(error.field(), Some(i18n_key::RDP_COLOR_DEPTH));
        }
        for desktop_scale in [0, 99, 501] {
            let error = RdpConfig {
                desktop_scale: Some(desktop_scale),
                ..valid.clone()
            }
            .validate()
            .unwrap_err();
            assert_eq!(error.field(), Some(i18n_key::RDP_REMOTE_DESKTOP_SCALING));
        }
        for max_fps in [0, 29, 31, 59, 61] {
            let error = RdpConfig {
                max_fps,
                ..valid.clone()
            }
            .validate()
            .unwrap_err();
            assert_eq!(error.field(), Some(i18n_key::RDP_LOCAL_REFRESH_LIMIT));
        }
        for desktop_scale in [None, Some(100), Some(175), Some(500)] {
            RdpConfig {
                desktop_scale,
                color_depth: 16,
                max_fps: 30,
                ..valid.clone()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn rdp_validation_checks_identity_resolution_timeout_and_trust() {
        let valid = RdpConfig {
            username: "operator".into(),
            ..RdpConfig::default()
        };
        valid.validate().unwrap();
        assert!(RdpConfig::default().validate().is_err());
        for invalid in [
            RdpConfig {
                host: "rdp://user:password@host".into(),
                ..valid.clone()
            },
            RdpConfig {
                port: 0,
                ..valid.clone()
            },
            RdpConfig {
                username: " \t".into(),
                ..valid.clone()
            },
            RdpConfig {
                username: "bad\nuser".into(),
                ..valid.clone()
            },
            RdpConfig {
                domain: Some(String::new()),
                ..valid.clone()
            },
            RdpConfig {
                domain: Some("bad\ndomain".into()),
                ..valid.clone()
            },
            RdpConfig {
                width: 319,
                ..valid.clone()
            },
            RdpConfig {
                width: 3841,
                ..valid.clone()
            },
            RdpConfig {
                height: 199,
                ..valid.clone()
            },
            RdpConfig {
                height: 2161,
                ..valid.clone()
            },
            RdpConfig {
                timeout_secs: 0,
                ..valid.clone()
            },
            RdpConfig {
                timeout_secs: 601,
                ..valid.clone()
            },
            RdpConfig {
                credential: Some(CredentialRef(Uuid::nil())),
                ..valid.clone()
            },
            RdpConfig {
                certificate_sha256: Some("ab".repeat(31)),
                ..valid.clone()
            },
            RdpConfig {
                certificate_sha256: Some("xy".repeat(32)),
                ..valid.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        RdpConfig {
            host: "[::1]".into(),
            width: 3840,
            height: 2160,
            ..valid
        }
        .validate()
        .unwrap();
    }
    #[test]
    fn remote_app_profiles_round_trip_without_changing_authentication_identity() {
        let desktop = RdpConfig {
            username: "operator".into(),
            ..Default::default()
        };
        let application = RdpConfig {
            remote_app: Some(RdpRemoteApp {
                program: "||notepad".into(),
                arguments: "文档.txt".into(),
                working_directory: "C:\\Docs".into(),
            }),
            ..desktop.clone()
        };
        application.validate().unwrap();
        RdpConfig {
            width: 3840,
            monitor_count: 4,
            ..application.clone()
        }
        .validate()
        .unwrap();
        assert!(application.same_auth_identity(&desktop));
        assert!(!application.uses_dynamic_resolution());
        let serialized = serde_json::to_string(&application).unwrap();
        assert_eq!(
            serde_json::from_str::<RdpConfig>(&serialized).unwrap(),
            application
        );
        assert!(
            serde_json::from_str::<RdpConfig>("{}")
                .unwrap()
                .remote_app
                .is_none()
        );
    }

    #[test]
    fn remote_app_uses_utf16_limits_and_rejects_empty_programs() {
        for program in [
            String::new(),
            " ".into(),
            "||name\0extra".into(),
            "😀".repeat(131),
        ] {
            let config = RdpConfig {
                remote_app: Some(RdpRemoteApp {
                    program,
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }
        let app = RdpRemoteApp {
            program: "||app".into(),
            arguments: "😀".repeat(4000),
            ..Default::default()
        };
        app.validate().unwrap();
        assert!(
            RdpRemoteApp {
                arguments: "😀".repeat(4001),
                ..app
            }
            .validate()
            .is_err()
        );
    }
}
