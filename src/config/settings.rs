use crate::i18n::messages as i18n_key;

mod theme;
pub use theme::{AppTheme, Theme};

use super::{Result, invalid, validate_text};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppLockConfig {
    pub password_hash: Option<String>,
    pub idle_minutes: u32,
}

impl AppLockConfig {
    pub fn validate(&self) -> Result<()> {
        if self.idle_minutes > 1440 || (self.password_hash.is_none() && self.idle_minutes != 0) {
            return Err(invalid(
                i18n_key::SETTINGS_INVALID_APPLICATION_LOCK_SETTINGS,
            ));
        }
        if let Some(hash) = &self.password_hash {
            crate::app_lock::validate(hash)
                .map_err(|_| invalid(i18n_key::SETTINGS_INVALID_APPLICATION_LOCK_SETTINGS))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub restore_workspace: bool,
    pub auto_reconnect: bool,
    pub status_bar_monitoring: bool,
    pub check_updates: bool,
    pub update_proxy: String,
    pub language: crate::i18n::Language,
    pub app_theme: AppTheme,
    pub font_family: String,
    pub font_size: f32,
    /// Cell height multiplier, shared by rendering and pointer/IME positioning.
    pub line_height: f32,
    pub line_numbers: bool,
    pub line_timestamps: bool,
    pub cache_command_output: bool,
    pub highlight: bool,
    pub highlight_selection_matches: bool,
    pub history_lines: usize,
    pub history_bytes: usize,
    pub global_history_bytes: usize,
    pub confirm_multiline_paste: bool,
    pub window_width: f32,
    pub window_height: f32,
    pub sidebar_visible: bool,
    pub sidebar_connection_details: bool,
    pub sidebar_width: f32,
    pub files_width: f32,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: crate::i18n::Language::System,
            app_theme: AppTheme::default(),
            restore_workspace: true,
            auto_reconnect: true,
            status_bar_monitoring: true,
            check_updates: true,
            update_proxy: String::new(),
            #[cfg(windows)]
            font_family: "Consolas".into(),
            #[cfg(not(windows))]
            font_family: "DejaVu Sans Mono".into(),
            font_size: 13.,
            line_height: 1.6,
            line_numbers: false,
            line_timestamps: true,
            cache_command_output: true,
            highlight: true,
            highlight_selection_matches: true,
            history_lines: 50_000,
            history_bytes: 64 * 1024 * 1024,
            global_history_bytes: 512 * 1024 * 1024,
            confirm_multiline_paste: true,
            window_width: 1100.,
            window_height: 740.,
            sidebar_visible: true,
            sidebar_connection_details: true,
            sidebar_width: 300.,
            files_width: 384.,
            theme: Theme::default(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        crate::update::validate_proxy(&self.update_proxy)
            .map_err(|_| invalid(i18n_key::UPDATES_INVALID_PROXY))?;
        validate_text(&self.font_family, 256, i18n_key::CONNECTIONS_FONT_FAMILY)?;
        if !(8.0..=40.0).contains(&self.font_size) || !(1.0..=3.0).contains(&self.line_height) {
            return Err(invalid(
                i18n_key::CONNECTIONS_FONT_SIZE_MUST_BE_8_TO_40_AND_LINE_HEIGHT,
            ));
        }
        if self.history_lines > 1_000_000
            || !(1024..=8 * 1024 * 1024 * 1024).contains(&self.history_bytes)
            || self.global_history_bytes < self.history_bytes
            || self.global_history_bytes > 64 * 1024 * 1024 * 1024
        {
            return Err(invalid(
                i18n_key::CONNECTIONS_INVALID_HISTORY_LINE_LIMIT_OR_MEMORY_BUDGET,
            ));
        }
        if !(480.0..=16384.0).contains(&self.window_width)
            || !(300.0..=16384.0).contains(&self.window_height)
        {
            return Err(invalid(
                i18n_key::CONNECTIONS_WINDOW_DIMENSIONS_ARE_OUT_OF_RANGE,
            ));
        }
        if !(180.0..=400.0).contains(&self.sidebar_width)
            || !(360.0..=800.0).contains(&self.files_width)
        {
            return Err(invalid(i18n_key::WORKSPACE_PANEL_WIDTHS_ARE_OUT_OF_RANGE));
        }
        self.theme.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::{AppTheme, Settings};

    #[test]
    fn application_theme_defaults_and_round_trips_without_changing_terminal_colors() {
        let mut settings: Settings =
            toml::from_str("font_size = 15.0\n[theme]\nbackground = 1193046").unwrap();
        assert_eq!(settings.app_theme, AppTheme::Dark);
        let terminal = settings.theme.clone();
        for theme in AppTheme::ALL {
            settings.app_theme = theme;
            let saved = toml::to_string(&settings).unwrap();
            let restored: Settings = toml::from_str(&saved).unwrap();
            restored.validate().unwrap();
            assert_eq!(restored.app_theme, theme);
            assert_eq!(restored.theme, terminal);
            assert_eq!(restored.font_size, 15.);
        }
        assert!(toml::from_str::<Settings>("app_theme = 'unknown'").is_err());
    }

    #[test]
    fn command_output_cache_defaults_on_and_saves_disabled_preference() {
        let mut settings: Settings = toml::from_str("font_size = 15.0").unwrap();
        assert!(settings.cache_command_output);
        settings.cache_command_output = false;
        let saved = toml::to_string(&settings).unwrap();
        let restored: Settings = toml::from_str(&saved).unwrap();
        assert!(!restored.cache_command_output);
    }

    #[test]
    fn status_monitoring_defaults_on_and_preserves_disabled_preference() {
        let mut settings: Settings = toml::from_str("font_size = 15.0").unwrap();
        assert!(settings.status_bar_monitoring);
        settings.status_bar_monitoring = false;
        let restored: Settings = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        assert!(!restored.status_bar_monitoring);
    }

    #[test]
    fn output_timestamps_default_on_and_save_independently_of_line_numbers() {
        let old: Settings = toml::from_str("line_numbers = false").unwrap();
        assert!(old.line_timestamps);
        assert!(!old.line_numbers);
        let saved = Settings {
            line_timestamps: false,
            line_numbers: true,
            ..old
        };
        let restored: Settings = toml::from_str(&toml::to_string(&saved).unwrap()).unwrap();
        assert!(!restored.line_timestamps);
        assert!(restored.line_numbers);
    }

    #[test]
    fn selection_highlighting_defaults_on_and_is_independent_of_syntax_highlighting() {
        let mut settings: Settings = toml::from_str("highlight = false").unwrap();
        assert!(settings.highlight_selection_matches);
        settings.highlight_selection_matches = false;
        settings.highlight = true;
        let restored: Settings = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        assert!(restored.highlight);
        assert!(!restored.highlight_selection_matches);
    }

    #[test]
    fn panel_preferences_default_for_existing_configs_and_reject_invalid_sizes() {
        let settings: Settings = toml::from_str("font_size = 15.0").unwrap();
        assert!(settings.sidebar_visible);
        assert!(settings.sidebar_connection_details);
        assert_eq!(settings.sidebar_width, 300.);
        assert_eq!(settings.files_width, 384.);
        for size in [f32::NAN, f32::INFINITY, 0., 401.] {
            assert!(
                Settings {
                    sidebar_width: size,
                    ..settings.clone()
                }
                .validate()
                .is_err()
            );
        }
        for size in [f32::NAN, f32::INFINITY, 359., 801.] {
            assert!(
                Settings {
                    files_width: size,
                    ..settings.clone()
                }
                .validate()
                .is_err()
            );
        }
        let encoded = toml::to_string(&Settings {
            sidebar_visible: false,
            sidebar_connection_details: false,
            sidebar_width: 300.,
            files_width: 600.,
            ..settings
        })
        .unwrap();
        let decoded: Settings = toml::from_str(&encoded).unwrap();
        assert!(!decoded.sidebar_visible);
        assert!(!decoded.sidebar_connection_details);
        assert_eq!((decoded.sidebar_width, decoded.files_width), (300., 600.));
    }

    #[test]
    fn language_settings_round_trip_with_stable_codes() {
        use crate::i18n::Language;
        for (language, code) in [
            (Language::System, "system"),
            (Language::English, "en"),
            (Language::SimplifiedChinese, "zh-CN"),
        ] {
            let settings = Settings {
                language,
                ..Settings::default()
            };
            let encoded = toml::to_string(&settings).unwrap();
            assert!(encoded.contains(&format!("language = \"{code}\"")));
            let decoded: Settings = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.language, language);
        }
    }
}
