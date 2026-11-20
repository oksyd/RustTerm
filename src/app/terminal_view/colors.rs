use rustterm::color::readable_on;
use rustterm::config::Theme;

#[derive(Clone, Copy)]
pub(super) struct OverlayColors {
    pub muted: u32,
    pub accent: u32,
    pub success: u32,
    pub danger: u32,
    pub hover: u32,
    pub on_hover: u32,
}

impl OverlayColors {
    pub fn new(theme: &Theme) -> Self {
        Self {
            muted: readable_on(theme.line_number, theme.background),
            accent: readable_on(theme.url, theme.background),
            success: readable_on(theme.info, theme.background),
            danger: readable_on(theme.error, theme.background),
            hover: theme.selection,
            on_hover: readable_on(theme.foreground, theme.selection),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_overlay_colors_follow_the_terminal_background() {
        for background in [Theme::default().background, 0xffffff, 0x888888, 0x00aa00] {
            let theme = Theme {
                background,
                selection: background ^ 0xffffff,
                ..Theme::default()
            };
            let colors = OverlayColors::new(&theme);
            for foreground in [colors.muted, colors.accent, colors.success, colors.danger] {
                assert!(
                    rustterm::color::contrast(foreground, background) >= 4.5,
                    "{foreground:06x} on {background:06x}"
                );
            }
            assert!(rustterm::color::contrast(colors.on_hover, colors.hover) >= 4.5);
        }
    }
}
