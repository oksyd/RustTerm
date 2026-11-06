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
        Self {
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
        }
    }
}

impl Theme {
    /// Refresh only the complete historical built-in palette. A single customized color
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
        if *self == legacy {
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
