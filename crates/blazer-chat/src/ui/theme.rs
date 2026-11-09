//! The semantic palette (m9): one [`Palette`] per window appearance,
//! painted through `rgb(u32)` like the rest of the crate. Contrast
//! pairs are asserted in tests against WCAG 2.1 relative luminance.

use gpui::WindowAppearance;

/// The window's colors as named u32 RGB values; `Copy` so render
/// inputs carry it by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) bg: u32,
    pub(crate) panel: u32,
    pub(crate) row_hover: u32,
    pub(crate) border: u32,
    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) accent: u32,
    /// Text painted on `accent` surfaces (selected chips, badges).
    pub(crate) accent_fg: u32,
    /// Error text, mention badges, logout.
    pub(crate) danger: u32,
    pub(crate) online: u32,
    pub(crate) away: u32,
    pub(crate) busy: u32,
    /// Inline code block background.
    pub(crate) code_bg: u32,
    /// Message input surface.
    pub(crate) input_bg: u32,
}

/// The shipped dark values; `border` and `busy` carry the m9 computed
/// contrast fixes (BORDER 1.83/1.62 → 3.57/3.16, BUSY 4.05/3.59 →
/// 6.59/5.84 against BG/PANEL).
pub(crate) const DARK: Palette = Palette {
    bg: 0x11151c,
    panel: 0x1a212b,
    row_hover: 0x1a2230,
    border: 0x5a6e95,
    text: 0xe8eaed,
    muted: 0x93a4bc,
    accent: 0x4f8cff,
    accent_fg: 0xffffff,
    danger: 0xe03131,
    online: 0x2f9e44,
    away: 0xe8b30f,
    busy: 0xff6b6b,
    code_bg: 0x1c2430,
    input_bg: 0x121820,
};

/// The light counterpart, designed to WCAG AA: every text pair ≥ 4.5:1
/// and both border pairs ≥ 3:1 (ratios in the m9 report).
pub(crate) const LIGHT: Palette = Palette {
    bg: 0xffffff,
    panel: 0xf6f8fa,
    row_hover: 0xeaeef2,
    border: 0x656d76,
    text: 0x1f2328,
    muted: 0x59636e,
    accent: 0x0865cd,
    accent_fg: 0xffffff,
    danger: 0xcf222e,
    online: 0x1a7f37,
    away: 0x9a6700,
    busy: 0xcf222e,
    code_bg: 0xeff2f6,
    input_bg: 0xfafbfc,
};

/// The palette for the window's current appearance; the Vibrant
/// variants map with their base appearance.
pub(crate) fn palette(appearance: WindowAppearance) -> Palette {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => LIGHT,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => DARK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance (sRGB linearisation) — the same
    /// formula the m9 journal computed with.
    fn luminance(rgb: u32) -> f64 {
        let channel = |shift: u32| {
            let c = f64::from((rgb >> shift) & 0xff) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (hi, lo) = (
            luminance(a).max(luminance(b)),
            luminance(a).min(luminance(b)),
        );
        (hi + 0.05) / (lo + 0.05)
    }

    fn assert_aa(fg: u32, surface: u32) {
        let ratio = contrast(fg, surface);
        assert!(
            ratio >= 4.5,
            "{fg:#06x} on {surface:#06x}: {ratio:.2} < 4.5"
        );
    }

    fn assert_ui(fg: u32, surface: u32) {
        let ratio = contrast(fg, surface);
        assert!(
            ratio >= 3.0,
            "{fg:#06x} on {surface:#06x}: {ratio:.2} < 3.0"
        );
    }

    #[test]
    fn theme_dark_border_holds_against_both_surfaces() {
        let vs_bg = contrast(DARK.border, DARK.bg);
        let vs_panel = contrast(DARK.border, DARK.panel);
        assert!(
            vs_bg >= 3.0 && vs_panel >= 3.0,
            "border must read as a UI edge on bg and panel: {vs_bg:.2}/{vs_panel:.2}"
        );
    }

    #[test]
    fn theme_dark_busy_reads_as_text_on_both_surfaces() {
        let vs_bg = contrast(DARK.busy, DARK.bg);
        let vs_panel = contrast(DARK.busy, DARK.panel);
        assert!(
            vs_bg >= 4.5 && vs_panel >= 4.5,
            "busy status must pass AA text on bg and panel: {vs_bg:.2}/{vs_panel:.2}"
        );
    }

    #[test]
    fn theme_light_text_holds_aa_on_every_surface() {
        for surface in [LIGHT.bg, LIGHT.panel, LIGHT.row_hover] {
            assert_aa(LIGHT.text, surface);
            assert_aa(LIGHT.muted, surface);
            assert_aa(LIGHT.accent, surface);
        }
        assert_aa(LIGHT.text, LIGHT.code_bg);
        assert_aa(LIGHT.text, LIGHT.input_bg);
        for surface in [LIGHT.bg, LIGHT.panel] {
            assert_aa(LIGHT.busy, surface);
        }
        assert_aa(LIGHT.accent_fg, LIGHT.accent);
        assert_aa(LIGHT.danger, LIGHT.bg);
        assert_aa(LIGHT.online, LIGHT.bg);
        assert_aa(LIGHT.away, LIGHT.bg);
    }

    #[test]
    fn theme_light_border_holds_against_both_surfaces() {
        assert_ui(LIGHT.border, LIGHT.bg);
        assert_ui(LIGHT.border, LIGHT.panel);
    }

    #[test]
    fn theme_palette_maps_both_appearances() {
        assert!(matches!(
            palette(WindowAppearance::Light),
            Palette { bg: 0xffffff, .. }
        ));
        assert!(matches!(
            palette(WindowAppearance::VibrantLight),
            Palette { bg: 0xffffff, .. }
        ));
        assert_eq!(palette(WindowAppearance::Dark), DARK);
        assert_eq!(palette(WindowAppearance::VibrantDark), DARK);
    }
}
