use gpui::App;
use rustterm::config::AppTheme;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub bg: u32,
    pub chrome: u32,
    pub surface: u32,
    pub raised: u32,
    pub border: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub on_accent: u32,
    pub brand: u32,
    pub success: u32,
    pub warning: u32,
    pub danger: u32,
    pub danger_bg: u32,
    pub hover: u32,
    pub selected: u32,
    pub status: u32,
    pub input: u32,
    pub blue: u32,
    pub teal: u32,
    pub pink: u32,
}

impl Colors {
    pub const fn for_theme(theme: AppTheme) -> Self {
        match theme {
            AppTheme::Dark => Self {
                bg: 0x1c1e22,
                chrome: 0x202226,
                surface: 0x23252a,
                raised: 0x292c32,
                border: 0x343840,
                text: 0xe3e5ea,
                muted: 0xa3a8b3,
                faint: 0x9da5b3,
                accent: 0xb5b8eb,
                accent_hover: 0xc3c6ed,
                on_accent: 0x1c1e22,
                brand: 0xd98c66,
                success: 0xa7c99c,
                warning: 0xe0b681,
                danger: 0xe39b9e,
                danger_bg: 0x3c2b30,
                hover: 0x2e3138,
                selected: 0x343644,
                status: 0x202226,
                input: 0x1c1e22,
                blue: 0x8bb6ca,
                teal: 0x9fcac4,
                pink: 0xd197b3,
            },
            AppTheme::Light => Self {
                bg: 0xf6f7fa,
                chrome: 0xeef0f5,
                surface: 0xffffff,
                raised: 0xffffff,
                border: 0xc7cdd8,
                text: 0x202632,
                muted: 0x535e70,
                faint: 0x5a6474,
                accent: 0x59429d,
                accent_hover: 0x473080,
                on_accent: 0xffffff,
                brand: 0x914021,
                success: 0x29623d,
                warning: 0x805516,
                danger: 0xa92c42,
                danger_bg: 0xfce9ed,
                hover: 0xe5e9f0,
                selected: 0xe1dff1,
                status: 0xeef0f5,
                input: 0xffffff,
                blue: 0x28657f,
                teal: 0x236860,
                pink: 0x8b3d68,
            },
            AppTheme::Midnight => Self {
                bg: 0x111827,
                chrome: 0x151e30,
                surface: 0x1a2539,
                raised: 0x202d43,
                border: 0x354761,
                text: 0xe0e9f5,
                muted: 0xa8b8d0,
                faint: 0xa2b3cc,
                accent: 0x9cb9ff,
                accent_hover: 0xb7ccff,
                on_accent: 0x111827,
                brand: 0xe7ad88,
                success: 0x9bcdaa,
                warning: 0xe8c183,
                danger: 0xf0a3b2,
                danger_bg: 0x3d273b,
                hover: 0x283650,
                selected: 0x2f3f5c,
                status: 0x151e30,
                input: 0x111827,
                blue: 0x82cad8,
                teal: 0x91cfc1,
                pink: 0xdba5cc,
            },
        }
    }

    pub fn group_colors(self) -> [u32; 6] {
        [
            self.accent,
            self.blue,
            self.success,
            self.warning,
            self.pink,
            self.brand,
        ]
    }
}

// GPUI constructs and paints UI elements on its foreground thread. Keep the
// active palette there so shared controls can read it without global locks or
// threading an App reference through every style helper. Background session
// tasks use their own terminal palette and never change application colors.
thread_local! {
    static ACTIVE: Cell<AppTheme> = const { Cell::new(AppTheme::Dark) };
}

pub fn colors() -> Colors {
    Colors::for_theme(ACTIVE.get())
}

pub fn set_theme(theme: AppTheme, cx: &mut App) {
    if ACTIVE.replace(theme) != theme {
        // Refresh child entities, popups and cached input text as well as the
        // workspace root, both when previewing and when restoring a choice.
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn contrast(a: u32, b: u32) -> f64 {
        let luminance = |color: u32| {
            [16, 8, 0]
                .into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(shift, weight)| {
                    let channel = f64::from((color >> shift) & 255) / 255.;
                    weight
                        * if channel <= 0.04045 {
                            channel / 12.92
                        } else {
                            ((channel + 0.055) / 1.055).powf(2.4)
                        }
                })
                .sum::<f64>()
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn theme_text_remains_readable_on_panels_selections_and_buttons() {
        for theme in AppTheme::ALL {
            let c = Colors::for_theme(theme);
            for background in [
                c.bg, c.chrome, c.surface, c.raised, c.input, c.selected, c.hover,
            ] {
                for foreground in [c.text, c.muted, c.faint] {
                    assert!(
                        contrast(foreground, background) >= 4.5,
                        "{theme:?}: {foreground:06x} on {background:06x}"
                    );
                }
            }
            for (foreground, background) in [
                (c.on_accent, c.accent),
                (c.on_accent, c.accent_hover),
                (c.danger, c.danger_bg),
            ] {
                assert!(
                    contrast(foreground, background) >= 4.5,
                    "{theme:?}: {foreground:06x} on {background:06x}"
                );
            }
        }
    }

    #[test]
    fn each_theme_keeps_six_distinct_readable_ssh_group_colors() {
        for theme in AppTheme::ALL {
            let c = Colors::for_theme(theme);
            let colors = c.group_colors();
            assert_eq!(colors.into_iter().collect::<HashSet<_>>().len(), 6);
            for foreground in colors {
                for background in [c.bg, c.chrome, c.hover] {
                    assert!(
                        contrast(foreground, background) >= 4.5,
                        "{theme:?}: {foreground:06x} on {background:06x}"
                    );
                }
            }
        }
    }
}
