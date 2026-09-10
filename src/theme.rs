//! Colours. Everything resolves through the terminal's own default colours
//! and ANSI palette, so hackney renders in whatever theme the terminal (or
//! the multiplexer it runs in) has. Nothing is pinned: the accent is the
//! palette's yellow, which most themes draw as their warm orange.
//!
//! As in rigor, the selection band and the hairlines are mixed from the
//! terminal's real foreground and background (see `probe`), so they sit inside
//! the theme instead of on top of it.

use ratatui::style::Color;

use crate::probe::{Probed, Rgb};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub fg: Color,
    pub bg: Color,
    pub accent: Color,
    pub muted: Color,
    pub border: Color,
    pub link: Color,
    pub code: Color,
    pub success: Color,
    pub failure: Color,
    pub pending: Color,
    pub sel_bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            accent: Color::Yellow,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            link: Color::Blue,
            code: Color::Green,
            success: Color::Green,
            failure: Color::Red,
            pending: Color::Yellow,
            sel_bg: Color::Reset,
        }
    }
}

impl Theme {
    pub fn resolve(probed: Probed) -> Self {
        Self::resolve_with(probed, no_color())
    }

    pub fn resolve_with(probed: Probed, no_color: bool) -> Self {
        if no_color {
            // Keep the layout, drop the hue: the bar glyph and weight carry
            // the selection on their own.
            return Self {
                accent: Color::Reset,
                muted: Color::Reset,
                border: Color::Reset,
                link: Color::Reset,
                code: Color::Reset,
                success: Color::Reset,
                failure: Color::Reset,
                pending: Color::Reset,
                ..Self::default()
            };
        }
        let mut t = Self::default();
        if let Some(bg) = probed.bg {
            let dark = luminance(bg) < 0.5;
            let toward = probed
                .fg
                .unwrap_or(if dark { (255, 255, 255) } else { (0, 0, 0) });
            t.sel_bg = to_color(mix(bg, toward, if dark { 0.11 } else { 0.075 }));
            t.border = to_color(mix(bg, toward, if dark { 0.22 } else { 0.18 }));
        }
        t
    }
}

fn to_color((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// `a` moved toward `b` by `t` (0 = a, 1 = b), per channel.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let ch = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Relative luminance (sRGB, WCAG), 0 for black to 1 for white.
fn luminance((r, g, b): Rgb) -> f32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every field, destructured so a new one fails to compile here until the
    /// tests below account for it.
    fn fields(t: &Theme) -> [Color; 11] {
        let Theme {
            fg,
            bg,
            accent,
            muted,
            border,
            link,
            code,
            success,
            failure,
            pending,
            sel_bg,
        } = *t;
        [
            fg, bg, accent, muted, border, link, code, success, failure, pending, sel_bg,
        ]
    }

    fn rgb(c: Color) -> Rgb {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected a mixed colour, got {other:?}"),
        }
    }

    fn channels(c: Rgb) -> [u8; 3] {
        [c.0, c.1, c.2]
    }

    fn any_rgb() -> impl Strategy<Value = Rgb> {
        any::<(u8, u8, u8)>()
    }

    fn between(x: u8, a: u8, b: u8) -> bool {
        a.min(b) <= x && x <= a.max(b)
    }

    proptest! {
        #[test]
        fn no_color_leaves_every_colour_to_the_terminal(
            fg in proptest::option::of(any_rgb()),
            bg in proptest::option::of(any_rgb()),
        ) {
            let t = Theme::resolve_with(Probed { fg, bg }, true);
            prop_assert_eq!(fields(&t), [Color::Reset; 11]);
        }

        #[test]
        fn without_a_known_background_the_palette_is_used_as_is(fg in proptest::option::of(any_rgb())) {
            let t = Theme::resolve_with(Probed { fg, bg: None }, false);
            prop_assert_eq!(fields(&t), fields(&Theme::default()));
        }

        #[test]
        fn only_the_selection_band_and_border_follow_the_background(
            fg in proptest::option::of(any_rgb()),
            bg in any_rgb(),
        ) {
            let t = Theme::resolve_with(Probed { fg, bg: Some(bg) }, false);
            let (mut got, mut want) = (fields(&t), fields(&Theme::default()));
            // border and sel_bg are fields 4 and 10
            for i in [4, 10] {
                prop_assert!(matches!(got[i], Color::Rgb(..)));
                got[i] = Color::Reset;
                want[i] = Color::Reset;
            }
            prop_assert_eq!(got, want);
        }

        #[test]
        fn the_band_and_border_sit_between_background_and_foreground(fg in any_rgb(), bg in any_rgb()) {
            let t = Theme::resolve_with(Probed { fg: Some(fg), bg: Some(bg) }, false);
            let (sel, border) = (channels(rgb(t.sel_bg)), channels(rgb(t.border)));
            for (i, (&b, &f)) in channels(bg).iter().zip(&channels(fg)).enumerate() {
                prop_assert!(between(sel[i], b, f) && between(border[i], b, f));
                // The hairline must stand out at least as much as the band.
                prop_assert!(border[i].abs_diff(b) >= sel[i].abs_diff(b));
            }
        }

        #[test]
        fn without_a_foreground_dark_backgrounds_lighten_and_light_ones_darken(bg in any_rgb()) {
            let t = Theme::resolve_with(Probed { fg: None, bg: Some(bg) }, false);
            let (sel, border, bg3) = (channels(rgb(t.sel_bg)), channels(rgb(t.border)), channels(bg));
            let sum = |c: [u8; 3]| c.iter().map(|&x| x as u32).sum::<u32>();
            if luminance(bg) < 0.5 {
                prop_assert!((0..3).all(|i| bg3[i] <= sel[i] && sel[i] <= border[i]));
                prop_assert!(sum(bg3) < sum(sel), "the band must be visible on {bg:?}");
            } else {
                prop_assert!((0..3).all(|i| bg3[i] >= sel[i] && sel[i] >= border[i]));
                prop_assert!(sum(bg3) > sum(sel), "the band must be visible on {bg:?}");
            }
        }

        #[test]
        fn mix_hits_its_ends_stays_between_them_and_moves_steadily(
            a in any_rgb(),
            b in any_rgb(),
            t1 in 0.0f32..=1.0,
            t2 in 0.0f32..=1.0,
        ) {
            prop_assert_eq!(mix(a, b, 0.0), a);
            prop_assert_eq!(mix(a, b, 1.0), b);
            prop_assert_eq!(mix(a, a, t1), a);
            let (lo, hi) = (t1.min(t2), t1.max(t2));
            let (near, far) = (channels(mix(a, b, lo)), channels(mix(a, b, hi)));
            for (i, (&x, &y)) in channels(a).iter().zip(&channels(b)).enumerate() {
                prop_assert!(between(near[i], x, y) && between(far[i], x, y));
                prop_assert!(far[i].abs_diff(x) >= near[i].abs_diff(x), "t={hi} is nearer {x} than t={lo}");
            }
        }

        #[test]
        fn luminance_stays_within_zero_and_one(c in any_rgb()) {
            let l = luminance(c);
            prop_assert!((0.0..=1.0 + 1e-6).contains(&l), "{c:?} -> {l}");
        }
    }

    #[test]
    fn luminance_runs_from_black_to_white_and_rises_with_every_channel() {
        assert_eq!(luminance((0, 0, 0)), 0.0);
        assert!((luminance((255, 255, 255)) - 1.0).abs() < 1e-6);
        // WCAG reference points on both sides of the linear toe.
        assert!((luminance((10, 10, 10)) - 0.003035).abs() < 1e-5);
        assert!((luminance((64, 64, 64)) - 0.051269).abs() < 1e-5);
        assert!((luminance((128, 128, 128)) - 0.215861).abs() < 1e-5);
        let sweeps: [fn(u8) -> Rgb; 3] = [|v| (v, 0, 0), |v| (0, v, 0), |v| (0, 0, v)];
        for sweep in sweeps {
            for v in 1..=255u8 {
                assert!(
                    luminance(sweep(v)) > luminance(sweep(v - 1)),
                    "{:?}",
                    sweep(v)
                );
            }
        }
        // Green reads brightest and blue darkest, as the eye sees them.
        assert!(luminance((0, 255, 0)) > luminance((255, 0, 0)));
        assert!(luminance((255, 0, 0)) > luminance((0, 0, 255)));
    }

    #[test]
    fn black_and_white_backgrounds_mix_toward_the_opposite_end() {
        let on = |bg| {
            Theme::resolve_with(
                Probed {
                    fg: None,
                    bg: Some(bg),
                },
                false,
            )
        };
        let dark = on((0, 0, 0));
        assert!(rgb(dark.sel_bg).0 > 0 && rgb(dark.border).0 > rgb(dark.sel_bg).0);
        let light = on((255, 255, 255));
        assert!(rgb(light.sel_bg).0 < 255 && rgb(light.border).0 < rgb(light.sel_bg).0);
    }

    #[test]
    fn a_probed_foreground_is_mixed_toward_rather_than_white_or_black() {
        // Dark background, orange text: the band warms instead of greying.
        let t = Theme::resolve_with(
            Probed {
                fg: Some((255, 128, 0)),
                bg: Some((0, 0, 32)),
            },
            false,
        );
        let (r, g, b) = rgb(t.sel_bg);
        assert!(r > g && b < 32, "{:?}", (r, g, b));
    }
}
