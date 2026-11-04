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
            foreground: 0xbac6d1,
            background: 0x202124,
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
