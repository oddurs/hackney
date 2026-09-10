//! Small helpers: epoch time, human-scale ages, width-aware truncation.

use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compact age: `12s`, `4m`, `3h`, `2d`, `5w`, `3y`.
pub fn rel_time(ts: i64, now: i64) -> String {
    let d = now.saturating_sub(ts).max(0);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else if d < 86400 * 14 {
        format!("{}d", d / 86400)
    } else if d < 86400 * 365 {
        format!("{}w", d / (86400 * 7))
    } else {
        format!("{}y", d / (86400 * 365))
    }
}

/// Without control characters: ratatui drops them when drawing, so leaving
/// them in would put every width computed here out of step with the screen.
pub fn clean(mut s: String) -> String {
    if s.contains(char::is_control) {
        s.retain(|c| !c.is_control());
    }
    s
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Per character, for tests that build text whose width adds up; drawing
/// measures by grapheme.
#[cfg(test)]
pub fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Truncate to `max` display columns, appending `…` when it had to cut.
/// Measured by grapheme, as the terminal draws it: `❤️` is one character
/// plus a selector, and two columns, not one.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = width(g);
        if w + gw > max - 1 {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    // A cut just after a word would leave `goes …`.
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// Pad to exactly `n` display columns (truncating when too long).
pub fn pad(s: &str, n: usize) -> String {
    let t = truncate(s, n);
    let w = width(&t);
    format!("{t}{}", " ".repeat(n.saturating_sub(w)))
}

/// A column followed by more text: truncated one short of `n` so it always
/// keeps a gutter.
pub fn cell(s: &str, n: usize) -> String {
    pad(&truncate(s, n.saturating_sub(1)), n)
}

/// Right-aligned in `n` columns — for numbers read down a column.
pub fn right(s: &str, n: usize) -> String {
    let t = truncate(s, n);
    format!("{}{t}", " ".repeat(n.saturating_sub(width(&t))))
}

pub fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::select;

    /// Text as it reaches these helpers: already `clean`, and without the
    /// emoji sequences whose string width is not the sum of their characters'
    /// (those have a test of their own).
    const CHARS: &[char] = &[
        'a', 'b', 'Z', '0', ' ', '.', '…', 'é', 'ß', '中', '字', 'あ', '한', '！', '😀', '🦀',
        '\u{301}', '\u{308}', '\u{200B}', '\u{AD}', '\u{3000}',
    ];

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(select(CHARS), 0..30).prop_map(|v| v.into_iter().collect())
    }

    const EMOJI: &[&str] = &[
        "\u{2764}\u{FE0F}",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F1EE}\u{1F1F8}",
        "\u{1F980}",
        "a",
        "b",
        " ",
        "\u{4E2D}",
    ];

    const MINUTE: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    const WEEK: i64 = 7 * DAY;
    const YEAR: i64 = 365 * DAY;

    /// `rel_time`'s output read back as (count, seconds per unit).
    fn read_age(s: &str) -> (i64, i64) {
        let (n, unit) = s.split_at(s.len() - 1);
        let secs = match unit {
            "s" => 1,
            "m" => MINUTE,
            "h" => HOUR,
            "d" => DAY,
            "w" => WEEK,
            "y" => YEAR,
            _ => panic!("unknown unit in {s:?}"),
        };
        (n.parse().expect(s), secs)
    }

    proptest! {
        #[test]
        fn truncate_fits_and_is_a_prefix_plus_ellipsis_when_cut(s in text(), max in 0usize..40) {
            let t = truncate(&s, max);
            prop_assert!(width(&t) <= max, "{t:?} is {} wide, max {max}", width(&t));
            if max == 0 {
                prop_assert_eq!(t, "");
            } else if width(&s) <= max {
                prop_assert_eq!(t, s);
            } else {
                let kept = t.strip_suffix('…').expect("a cut ends in an ellipsis");
                prop_assert!(s.starts_with(kept) && kept.len() < s.len(), "{kept:?} is not a proper prefix of {s:?}");
            }
        }

        #[test]
        fn truncate_cuts_as_late_as_it_can(s in text(), max in 1usize..40) {
            let t = truncate(&s, max);
            if let Some(kept) = t.strip_suffix('…').filter(|_| t != s) {
                // Whitespace at the cut is dropped rather than kept before the
                // ellipsis, so what didn't fit is the next thing after it.
                let rest = &s[kept.len()..];
                let gap = &rest[..rest.len() - rest.trim_start().len()];
                if let Some(next) = rest.trim_start().graphemes(true).next() {
                    prop_assert!(width(kept) + width(gap) + width(next) > max - 1, "{next:?} would still fit after {kept:?}");
                }
            }
        }

        #[test]
        fn pad_fills_exactly_n_columns_after_the_truncated_text(s in text(), n in 0usize..40) {
            let p = pad(&s, n);
            prop_assert_eq!(width(&p), n);
            let t = truncate(&s, n);
            let tail = p.strip_prefix(t.as_str()).expect("starts with the truncated text");
            prop_assert!(tail.chars().all(|c| c == ' '));
        }

        #[test]
        fn cell_fills_exactly_n_columns_and_always_leaves_a_gutter(s in text(), n in 0usize..40) {
            let c = cell(&s, n);
            prop_assert_eq!(width(&c), n);
            if n > 0 {
                prop_assert!(c.ends_with(' '), "{c:?} has no gutter");
                prop_assert!(c.starts_with(&truncate(&s, n - 1)));
            }
        }

        #[test]
        fn right_fills_exactly_n_columns_and_ends_with_the_text(s in text(), n in 0usize..40) {
            let r = right(&s, n);
            prop_assert_eq!(width(&r), n);
            let lead = r.strip_suffix(truncate(&s, n).as_str()).expect("ends with the truncated text");
            prop_assert!(lead.chars().all(|c| c == ' '));
        }

        #[test]
        fn rel_time_floors_to_one_unit_and_never_goes_backwards(
            age in 0i64..40 * YEAR,
            later in 0i64..2 * YEAR,
            now in 0i64..4_000_000_000,
        ) {
            let (n, unit) = read_age(&rel_time(now - age, now));
            prop_assert!(n * unit <= age && age < (n + 1) * unit, "{age}s read as {n}x{unit}");
            let limit = match unit { 1 | MINUTE => 60, HOUR => 24, DAY => 14, WEEK => 53, _ => i64::MAX };
            prop_assert!(n < limit, "{n} of unit {unit} should have rolled over");
            let (m, unit2) = read_age(&rel_time(now - age - later, now));
            prop_assert!(m * unit2 >= n * unit, "{}s older reads younger", later);
        }

        #[test]
        fn a_timestamp_in_the_future_reads_as_zero_seconds(ahead in 1i64..1_000_000_000_000, now in -1_000_000_000i64..4_000_000_000) {
            prop_assert_eq!(rel_time(now + ahead, now), "0s");
        }

        #[test]
        fn clean_drops_control_characters_and_keeps_everything_else_in_order(
            s in prop::collection::vec(
                prop_oneof![any::<char>(), select(&['\0', '\x1b', '\x7f', '\u{85}', '\u{9b}', '\n', 'a', 'é'][..])],
                0..30,
            )
            .prop_map(|v| v.into_iter().collect::<String>()),
        ) {
            let c = clean(s.clone());
            prop_assert!(!c.contains(char::is_control));
            let visible: String = s.chars().filter(|ch| !ch.is_control()).collect();
            prop_assert_eq!(&c, &visible);
            prop_assert_eq!(clean(c.clone()), c);
        }

        /// Well-formed emoji sequences, each one grapheme drawn two columns
        /// wide, mixed with plain text: measured and cut as drawn.
        #[test]
        fn truncate_measures_emoji_sequences_as_drawn(
            parts in prop::collection::vec(select(EMOJI), 0..12),
            max in 0usize..30,
        ) {
            let s: String = parts.concat();
            let t = truncate(&s, max);
            prop_assert!(width(&t) <= max, "{t:?} is {} wide, max {max}", width(&t));
            if let Some(kept) = t.strip_suffix('…').filter(|_| t != s) {
                prop_assert!(s.starts_with(kept), "{kept:?} is not a prefix of {s:?}");
            }
        }

        #[test]
        fn plural_uses_the_singular_for_exactly_one(n in prop_oneof![0u32..3, any::<u32>()]) {
            let want = if n == 1 { format!("{n} point") } else { format!("{n} points") };
            prop_assert_eq!(plural(n, "point", "points"), want);
        }
    }

    #[test]
    fn zero_columns_leave_nothing() {
        for s in ["", "a", "中", "hello"] {
            assert_eq!(truncate(s, 0), "");
            assert_eq!(pad(s, 0), "");
            assert_eq!(cell(s, 0), "");
            assert_eq!(right(s, 0), "");
        }
    }

    #[test]
    fn a_wide_character_that_straddles_the_cut_is_dropped_whole() {
        assert_eq!(truncate("a中b", 3), "a…");
        assert_eq!(pad("a中b", 3), "a… ");
        assert_eq!(truncate("中中中", 2), "…");
        assert_eq!(right("中中中", 2), " …");
        assert_eq!(cell("中文", 3), "…  ");
        assert_eq!(truncate("中中", 4), "中中", "exactly fits, so no cut");
    }

    #[test]
    fn combining_marks_stay_with_their_base_when_cut() {
        let e = "e\u{301}";
        assert_eq!(truncate(&e.repeat(3), 2), format!("{e}…"));
        assert_eq!(truncate(&e.repeat(3), 3), e.repeat(3));
    }

    #[test]
    fn rel_time_switches_units_exactly_at_the_boundaries() {
        let now = 1_700_000_000;
        for (age, want) in [
            (0, "0s"),
            (59, "59s"),
            (MINUTE, "1m"),
            (HOUR - 1, "59m"),
            (HOUR, "1h"),
            (DAY - 1, "23h"),
            (DAY, "1d"),
            (14 * DAY - 1, "13d"),
            (14 * DAY, "2w"),
            (364 * DAY, "52w"),
            (YEAR - 1, "52w"),
            (YEAR, "1y"),
            (10 * YEAR + 364 * DAY, "10y"),
        ] {
            assert_eq!(rel_time(now - age, now), want, "age {age}s");
        }
    }

    #[test]
    fn clean_leaves_ordinary_text_alone_and_strips_terminal_escapes() {
        assert_eq!(clean("héllo 中 😀".into()), "héllo 中 😀");
        assert_eq!(
            clean("a\x1b[31mb\u{7}\r\n\t\u{85}\u{9b}c".into()),
            "a[31mbc"
        );
    }

    // `❤` is one column, `❤` + VS16 is two. `truncate` measures the whole
    // string with `width` but cuts by summing `char_width`, which sees one
    // column per heart, so the cut keeps too much.
    #[test]
    fn truncate_stays_within_max_for_emoji_presentation_sequences() {
        let hearts = "\u{2764}\u{FE0F}".repeat(3);
        assert_eq!(width(&hearts), 6);
        let t = truncate(&hearts, 3);
        assert!(width(&t) <= 3, "{t:?} is {} wide", width(&t));
    }
}
