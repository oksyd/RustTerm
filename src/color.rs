//! Contrast for client-owned text and decorations. Remote ANSI colors stay authoritative.
/// Blend a decoration into its surface without changing either source color.
pub fn mix(background: u32, foreground: u32, foreground_percent: u8) -> u32 {
    let weight = u32::from(foreground_percent.min(100));
    [16, 8, 0].into_iter().fold(0, |color, shift| {
        let channel = (((background >> shift) & 255) * (100 - weight)
            + ((foreground >> shift) & 255) * weight)
            / 100;
        color | (channel << shift)
    })
}

pub fn contrast(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn luminance(color: u32) -> f64 {
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
        .sum()
}

/// Prefer the supplied text color; use the more legible neutral only when necessary.
pub fn readable_on(foreground: u32, background: u32) -> u32 {
    let (fg, bg) = (luminance(foreground), luminance(background));
    if (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05) >= 4.5 {
        foreground
    } else if bg > 0.179 {
        0x000000
    } else {
        0xffffff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_contrast_preserves_readable_text_and_handles_midtones() {
        assert_eq!(contrast(0, 0xffffff), 21.);
        assert_eq!(contrast(0x808080, 0x808080), 1.);
        assert_eq!(readable_on(0xbac6d1, 0x202124), 0xbac6d1);
        for background in [0, 0xffffff, 0x888888, 0x00aa00, 0xff3333, 0x123456] {
            assert!(contrast(readable_on(background, background), background) >= 4.5);
        }
    }
}
