//! The semantic palette (m9): one [`Palette`] per window appearance,
//! painted through `rgb(u32)` like the rest of the crate. Contrast
//! pairs are asserted in tests against WCAG 2.1 relative luminance.

/// The window's colors as named u32 RGB values; `Copy` so render
/// inputs carry it by value. The `dead_code` expectations hold until
/// the m9 sweep wires the remaining fields in.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) bg: u32,
    pub(crate) panel: u32,
    pub(crate) row_hover: u32,
    pub(crate) border: u32,
    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) accent: u32,
    #[expect(dead_code)]
    /// Text painted on `accent` surfaces (selected chips, badges).
    pub(crate) accent_fg: u32,
    #[expect(dead_code)]
    /// Error text, mention badges, logout.
    pub(crate) danger: u32,
    #[expect(dead_code)]
    pub(crate) online: u32,
    #[expect(dead_code)]
    pub(crate) away: u32,
    /// Only the contrast test reads `busy` before the m9 sweep.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) busy: u32,
    #[expect(dead_code)]
    /// Inline code block background.
    pub(crate) code_bg: u32,
    #[expect(dead_code)]
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
}
