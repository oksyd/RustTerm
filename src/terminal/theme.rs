// The default ANSI and semantic palettes share the same color families.
// OSC palette overrides and explicit truecolor output still take precedence.
pub(super) const ANSI: [u32; 16] = [
    0x161b22, 0xed9393, 0x9fca91, 0xe5bd78, 0x80b9e8, 0xbd9de3, 0x7acbb5, 0xbac6d1, 0x929bab,
    0xf0a3a3, 0xb3d7a7, 0xefd09a, 0x9acdf5, 0xd0b6ef, 0x9bdccc, 0xe3e5ea,
];

/// Display palette independent of configuration and GPUI. Explicit ANSI RGB/indexed
/// foregrounds are resolved before semantic decoration and remain authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub foreground: u32,
    pub background: u32,
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
            foreground: ANSI[7],
            background: 0x202124,
            selection: 0x4b6f97,
            cursor: 0xc3c6ed,
            line_number: ANSI[8],
            trace: ANSI[8],
            debug: ANSI[5],
            info: ANSI[2],
            warn: ANSI[3],
            error: ANSI[1],
            fatal: ANSI[9],
            timestamp: ANSI[8],
            address: 0xb5a5f0,
            url: 0x64c8ff,
            path: ANSI[6],
        }
    }
}

impl Theme {
    pub(super) fn bracket_colors(&self) -> [u32; 6] {
        [
            self.address,
            self.path,
            self.url,
            self.debug,
            crate::color::mix(self.path, self.foreground, 50),
            crate::color::mix(self.url, self.foreground, 50),
        ]
    }
}

impl super::Terminal {
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.highlight_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.version = self.version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_terminal_text_colors_are_readable() {
        let theme = Theme::default();
        for (name, color) in [
            ("text", theme.foreground),
            ("line numbers", theme.line_number),
            ("trace", theme.trace),
            ("debug", theme.debug),
            ("info", theme.info),
            ("warn", theme.warn),
            ("error", theme.error),
            ("fatal", theme.fatal),
            ("timestamp", theme.timestamp),
            ("address", theme.address),
            ("URL", theme.url),
            ("path", theme.path),
        ] {
            assert!(
                crate::color::contrast(color, theme.background) >= 4.5,
                "{name}: {:0.2}",
                crate::color::contrast(color, theme.background)
            );
        }
        assert!(
            crate::color::contrast(theme.selection, theme.background) >= 3.,
            "selection blends into the terminal background"
        );
        assert!(
            crate::color::contrast(
                crate::color::readable_on(0xffffff, theme.selection),
                theme.selection
            ) >= 4.5
        );
        for index in 1..16 {
            assert!(
                crate::color::contrast(super::super::palette(index), theme.background) >= 4.5,
                "ANSI {index}"
            );
        }
        let brackets = theme.bracket_colors();
        assert_eq!(
            brackets
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            6
        );
        for color in brackets {
            assert!(crate::color::contrast(color, theme.background) >= 4.5);
            assert!(![theme.warn, theme.error, theme.fatal].contains(&color));
        }
    }
}
