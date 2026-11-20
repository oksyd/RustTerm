use crate::i18n::messages as i18n_key;

use super::{Result, invalid};
use serde::{Deserialize, Serialize};

/// Application chrome, independent of the user's terminal palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppTheme {
    #[default]
    Dark,
    Light,
    Midnight,
}

impl AppTheme {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::Midnight];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => i18n_key::SETTINGS_THEME_DARK,
            Self::Light => i18n_key::SETTINGS_THEME_LIGHT,
            Self::Midnight => i18n_key::SETTINGS_THEME_MIDNIGHT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: u32,
    pub foreground: u32,
    pub selection: u32,
    pub cursor: u32,
    pub line_number: u32,
    pub trace: u32,
    pub debug: u32,
    pub info: u32,
    pub warn: u32,
    pub error: u32,
    pub fatal: u32,
    pub timestamp: u32,
    pub address: u32,
    pub url: u32,
    pub path: u32,
}

impl Default for Theme {
    fn default() -> Self {
        let palette = crate::terminal::Theme::default();
        Self {
            background: palette.background,
            foreground: palette.foreground,
            selection: palette.selection,
            cursor: palette.cursor,
            line_number: palette.line_number,
            trace: palette.trace,
            debug: palette.debug,
            info: palette.info,
            warn: palette.warn,
            error: palette.error,
            fatal: palette.fatal,
            timestamp: palette.timestamp,
            address: palette.address,
            url: palette.url,
            path: palette.path,
        }
    }
}

impl Theme {
    /// Refresh only complete historical built-in palettes. A single customized color
    /// preserves the entire user's theme; font preferences are deliberately unrelated.
    pub fn upgrade_legacy_default(&mut self) -> bool {
        let legacy = Self {
            background: 0x10151d,
            foreground: 0xcbd5e1,
            selection: 0x365373,
            cursor: 0xf8fafc,
            line_number: 0x64748b,
            trace: 0x64748b,
            debug: 0x94a3b8,
            info: 0x86efac,
            warn: 0xfacc15,
            error: 0xf87171,
            fatal: 0xfb7185,
            timestamp: 0x94a3b8,
            address: 0xc4b5fd,
            url: 0x7dd3fc,
            path: 0x5eead4,
        };
        let previous = Self {
            background: 0x202124,
            foreground: 0xbac6d1,
            selection: 0x3a3b46,
            cursor: 0xc3c6ed,
            line_number: 0x757c8d,
            trace: 0x7f8ca1,
            debug: 0xa9a2c3,
            info: 0xa2bca4,
            warn: 0xd4b57d,
            error: 0xdb949f,
            fatal: 0xe39b9e,
            timestamp: 0x7f8ca1,
            address: 0xb5b8eb,
            url: 0x9fb8cf,
            path: 0xa3acc8,
        };
        let previous_readable = Self {
            foreground: 0xbac6d1,
            background: 0x202124,
            selection: 0x4b6f97,
            cursor: 0xc3c6ed,
            line_number: 0x929bab,
            trace: 0x929bab,
            debug: 0xbd9de3,
            info: 0x9fca91,
            warn: 0xe5bd78,
            error: 0xed9393,
            fatal: 0xee91ba,
            timestamp: 0x929bab,
            address: 0xb5a5f0,
            url: 0x64c8ff,
            path: 0x7acbb5,
        };
        if *self == legacy || *self == previous || *self == previous_readable {
            *self = Self::default();
            true
        } else {
            false
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if [
            self.background,
            self.foreground,
            self.selection,
            self.cursor,
            self.line_number,
            self.trace,
            self.debug,
            self.info,
            self.warn,
            self.error,
            self.fatal,
            self.timestamp,
            self.address,
            self.url,
            self.path,
        ]
        .into_iter()
        .any(|c| c > 0xffffff)
        {
            return Err(invalid(
                i18n_key::CONNECTIONS_THEME_COLORS_MUST_BE_24_BIT_RGB_VALUES,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_readable_default_upgrades_without_replacing_custom_colors() {
        let previous = Theme {
            foreground: 0xbac6d1,
            background: 0x202124,
            selection: 0x4b6f97,
            cursor: 0xc3c6ed,
            line_number: 0x929bab,
            trace: 0x929bab,
            debug: 0xbd9de3,
            info: 0x9fca91,
            warn: 0xe5bd78,
            error: 0xed9393,
            fatal: 0xee91ba,
            timestamp: 0x929bab,
            address: 0xb5a5f0,
            url: 0x64c8ff,
            path: 0x7acbb5,
        };
        let mut upgraded = previous.clone();
        assert!(upgraded.upgrade_legacy_default());
        assert_eq!(upgraded, Theme::default());
        assert!(!upgraded.upgrade_legacy_default());
        let mut custom = Theme {
            url: 0xabcdef,
            ..previous
        };
        let expected = custom.clone();
        assert!(!custom.upgrade_legacy_default());
        assert_eq!(custom, expected);
    }

    #[test]
    fn stored_default_upgrades_once_and_custom_palettes_remain_intact() {
        let saved = r#"
background = 2105636
foreground = 12240593
selection = 3816262
cursor = 12830445
line_number = 7699597
trace = 8359073
debug = 11117251
info = 10665124
warn = 13940093
error = 14390431
fatal = 14916510
timestamp = 8359073
address = 11909355
url = 10467535
path = 10726600
"#;
        let original: Theme = toml::from_str(saved).unwrap();
        let mut upgraded = original.clone();
        assert!(upgraded.upgrade_legacy_default());
        assert_eq!(upgraded, Theme::default());
        assert!(!upgraded.upgrade_legacy_default());
        let mut custom = original;
        custom.url = 0xabcdef;
        let expected = custom.clone();
        assert!(!custom.upgrade_legacy_default());
        assert_eq!(custom, expected);
    }
}
