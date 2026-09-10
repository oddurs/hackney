//! Ask the terminal which colours it is actually drawing with.
//!
//! OSC 10 and OSC 11 report the default foreground and background. With those,
//! the selection band and the hairlines can be mixed from the user's real theme
//! instead of pinned to a palette slot — a fixed slot is what made the old
//! highlight look off against a tinted background.
//!
//! A DA1 query goes last because every terminal answers it. Replies arrive in
//! order, so once DA1's reply is in, a terminal that has not answered OSC 10/11
//! never will: unsupported terminals cost a round trip, not a timeout.

use std::time::Duration;

pub type Rgb = (u8, u8, u8);

/// A reply the probe stopped reading partway through, by the kind of escape
/// sequence it was: the rest is still on its way, and is not typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Ends with ST or BEL.
    Osc,
    /// Ends with a final byte, `@` to `~`.
    Csi,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Probed {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
}

/// Must run in raw mode, before anything else reads stdin, so the replies are
/// neither echoed nor handed to the event loop as keystrokes.
#[cfg(unix)]
pub fn query(timeout: Duration) -> (Probed, Option<Cut>) {
    use std::io::Write;
    use std::time::Instant;

    // Only meaningful when both ends are a terminal.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        return (Probed::default(), None);
    }
    let mut out = std::io::stdout();
    if out
        .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c")
        .and_then(|_| out.flush())
        .is_err()
    {
        return (Probed::default(), None);
    }

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    while !has_da1(&buf) {
        // Never stop halfway through a reply: its tail would reach the event
        // loop as keystrokes, with no `ESC ]` left to mark it as a reply.
        let due = if mid_reply(&buf).is_some() {
            deadline + GRACE
        } else {
            deadline
        };
        let left = due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            break;
        }
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    (parse(&buf), mid_reply(&buf))
}

/// How long past the deadline to wait for the rest of a reply already begun.
#[cfg(unix)]
const GRACE: Duration = Duration::from_millis(500);

/// Whether `buf` ends inside an escape sequence, and which kind: an OSC
/// reply without its BEL or ST, or a CSI reply (DA1) without its final byte.
/// A lone ESC is taken for the start of an OSC or of its ST.
fn mid_reply(buf: &[u8]) -> Option<Cut> {
    let esc = buf.iter().rposition(|&b| b == 0x1b)?;
    let tail = &buf[esc..];
    match tail.get(1) {
        None => Some(Cut::Osc),
        Some(b']') => (!tail.contains(&0x07)).then_some(Cut::Osc),
        Some(b'[') => (!tail[2..].iter().any(|b| (0x40..=0x7e).contains(b))).then_some(Cut::Csi),
        // `ESC \` ends an OSC; anything else is not ours to wait for.
        _ => None,
    }
}

#[cfg(not(unix))]
pub fn query(_timeout: Duration) -> (Probed, Option<Cut>) {
    (Probed::default(), None)
}

/// `ESC [ ? <digits and semicolons> c` — the Primary Device Attributes reply.
fn has_da1(buf: &[u8]) -> bool {
    let mut from = 0;
    while let Some(p) = buf[from..].windows(3).position(|w| w == b"\x1b[?") {
        let mut i = from + p + 3;
        while i < buf.len() && (buf[i].is_ascii_digit() || buf[i] == b';') {
            i += 1;
        }
        if buf.get(i) == Some(&b'c') {
            return true;
        }
        from += p + 3;
    }
    false
}

pub fn parse(buf: &[u8]) -> Probed {
    let s = String::from_utf8_lossy(buf);
    Probed {
        fg: reply(&s, "\x1b]10;"),
        bg: reply(&s, "\x1b]11;"),
    }
}

/// The colour in an OSC reply, terminated by BEL or by ST (`ESC \`).
fn reply(s: &str, prefix: &str) -> Option<Rgb> {
    let rest = &s[s.find(prefix)? + prefix.len()..];
    // Unterminated means cut off by the timeout: `rgb:1616/1818/1` is not a
    // colour to guess from.
    let end = rest.find(['\x07', '\x1b'])?;
    parse_rgb(&rest[..end])
}

/// `rgb:RRRR/GGGG/BBBB`, one to four hex digits per channel (X11 colour spec).
fn parse_rgb(spec: &str) -> Option<Rgb> {
    let mut channels = spec.strip_prefix("rgb:")?.split('/');
    let mut next = || -> Option<u8> {
        let hex = channels.next()?;
        if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    };
    Some((next()?, next()?, next()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_colours_terminated_by_st() {
        let r =
            parse(b"\x1b]10;rgb:d4d4/d8d8/dede\x1b\\\x1b]11;rgb:1616/1818/1c1c\x1b\\\x1b[?62;22c");
        assert_eq!(r.fg, Some((0xd4, 0xd8, 0xde)));
        assert_eq!(r.bg, Some((0x16, 0x18, 0x1c)));
    }

    #[test]
    fn reads_bel_terminated_and_short_channels() {
        let r = parse(b"\x1b]11;rgb:fb/fb/fa\x07\x1b[?1;2c");
        assert_eq!(r.bg, Some((0xfb, 0xfb, 0xfa)));
        assert_eq!(r.fg, None);
        // one hex digit per channel scales across the full range
        assert_eq!(parse_rgb("rgb:f/8/0"), Some((255, 136, 0)));
    }

    #[test]
    fn a_terminal_that_only_answers_da1_yields_nothing() {
        let buf = b"\x1b[?62;4c";
        assert!(has_da1(buf));
        assert_eq!(parse(buf), Probed::default());
    }

    #[test]
    fn da1_is_not_mistaken_for_a_partial_reply() {
        assert!(!has_da1(b"\x1b]11;rgb:16/18/1c\x1b\\"));
        assert!(!has_da1(b"\x1b[?62;4"), "incomplete: no final `c` yet");
    }

    #[test]
    fn malformed_specs_are_rejected_not_guessed() {
        assert_eq!(parse_rgb("rgb:12345/00/00"), None);
        assert_eq!(parse_rgb("rgb:zz/00/00"), None);
        assert_eq!(parse_rgb("#161818"), None);
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::select;

    /// The X11 scaling, worked in floating point: `v` out of `16^digits - 1`,
    /// to the nearest step of 255. No value lands on a half, so rounding mode
    /// cannot matter.
    fn scaled(v: u32, digits: u32) -> u8 {
        let max = 16f64.powi(digits as i32) - 1.0;
        (v as f64 * 255.0 / max).round() as u8
    }

    /// One channel: (hex text, the byte it should become).
    fn channel() -> impl Strategy<Value = (String, u8)> {
        (1u32..=4, any::<u32>(), any::<bool>()).prop_map(|(digits, v, upper)| {
            let v = v % 16u32.pow(digits);
            let hex = if upper {
                format!("{v:0w$X}", w = digits as usize)
            } else {
                format!("{v:0w$x}", w = digits as usize)
            };
            (hex, scaled(v, digits))
        })
    }

    /// An OSC 10/11 reply: (bytes, colour it carries).
    fn osc_reply(code: u8) -> impl Strategy<Value = (Vec<u8>, Rgb)> {
        (channel(), channel(), channel(), any::<bool>()).prop_map(move |(r, g, b, bel)| {
            let end = if bel { "\x07" } else { "\x1b\\" };
            let bytes = format!("\x1b]{code};rgb:{}/{}/{}{end}", r.0, g.0, b.0);
            (bytes.into_bytes(), (r.1, g.1, b.1))
        })
    }

    fn da1() -> impl Strategy<Value = Vec<u8>> {
        "[0-9]{1,3}(;[0-9]{1,3}){0,5}".prop_map(|p| format!("\x1b[?{p}c").into_bytes())
    }

    /// Bytes a terminal might send around the replies: typed keys, other
    /// escape sequences. Never an OSC 10/11 of its own.
    fn noise() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop_oneof![
                "[ -~]{0,6}".prop_map(String::into_bytes),
                select(
                    &[
                        "\x1b[A",
                        "\x1b[1;5C",
                        "\x1bOP",
                        "\x1b]4;1;rgb:ff/00/00\x07",
                        "\x1b[?1049h",
                        "\r\n",
                        "\x1b[?62;4"
                    ][..]
                )
                .prop_map(|s| s.as_bytes().to_vec()),
            ],
            0..4,
        )
        .prop_map(|v| v.concat())
    }

    /// Fragments of replies, to be shuffled into near misses.
    const REPLY_PARTS: [&[u8]; 18] = [
        b"\x1b]10;",
        b"\x1b]11;",
        b"rgb:",
        b"/",
        b"f",
        b"0",
        b"fF",
        b"ffff",
        b"12345",
        b"\x07",
        b"\x1b\\",
        b"\x1b",
        b"\xff",
        b"\xc3",
        b"\xe2\x82",
        b"\x1b[?",
        b"1;2",
        b"c",
    ];

    /// The reference reading of DA1: at some `ESC [ ?`, only digits and
    /// semicolons up to a `c`. Brute force, so it shares nothing with the scan.
    fn has_da1_slow(buf: &[u8]) -> bool {
        (0..buf.len()).any(|i| {
            buf[i..].starts_with(b"\x1b[?")
                && (i + 3..buf.len()).any(|j| {
                    buf[j] == b'c'
                        && buf[i + 3..j]
                            .iter()
                            .all(|b| b.is_ascii_digit() || *b == b';')
                })
        })
    }

    proptest! {
        #[test]
        fn parse_never_panics_on_arbitrary_bytes(buf in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse(&buf);
            let _ = has_da1(&buf);
        }

        #[test]
        fn parse_never_panics_on_bytes_close_to_a_reply(
            parts in prop::collection::vec(select(&REPLY_PARTS[..]), 0..24),
        ) {
            let buf = parts.concat();
            let _ = parse(&buf);
            let _ = has_da1(&buf);
        }

        #[test]
        fn replies_are_read_in_any_order_with_either_terminator_amid_noise(
            fg in osc_reply(10),
            bg in osc_reply(11),
            da in da1(),
            gaps in prop::collection::vec(noise(), 4),
            order in Just([0usize, 1, 2]).prop_shuffle(),
        ) {
            let parts = [&fg.0, &bg.0, &da];
            let mut buf = gaps[0].clone();
            for (k, &i) in order.iter().enumerate() {
                buf.extend_from_slice(parts[i]);
                buf.extend_from_slice(&gaps[k + 1]);
            }
            prop_assert_eq!(parse(&buf), Probed { fg: Some(fg.1), bg: Some(bg.1) });
            prop_assert!(has_da1(&buf));
        }

        #[test]
        fn a_colour_that_was_never_answered_stays_unknown(
            reply in osc_reply(11),
            da in da1(),
            before in noise(),
        ) {
            let buf = [before, reply.0, da].concat();
            prop_assert_eq!(parse(&buf), Probed { fg: None, bg: Some(reply.1) });
        }

        #[test]
        fn da1_is_found_anywhere_in_arbitrary_noise(
            before in prop::collection::vec(any::<u8>(), 0..64),
            da in da1(),
            after in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            prop_assert!(has_da1(&[before, da, after].concat()));
        }

        #[test]
        fn no_strict_prefix_of_a_da1_reply_counts_as_one(da in da1(), before in "[ -~]{0,8}") {
            let buf = [before.into_bytes(), da].concat();
            for end in 0..buf.len() {
                prop_assert!(!has_da1(&buf[..end]), "{:?}", String::from_utf8_lossy(&buf[..end]));
            }
            prop_assert!(has_da1(&buf));
        }

        #[test]
        fn has_da1_agrees_with_a_brute_force_reading(
            buf in prop::collection::vec(select(&b"\x1b[?;09cx"[..]), 0..24),
        ) {
            prop_assert_eq!(has_da1(&buf), has_da1_slow(&buf));
        }
    }

    #[test]
    fn every_channel_value_of_every_width_scales_by_the_x11_formula() {
        for digits in 1..=4u32 {
            let max = 16u32.pow(digits) - 1;
            let mut last = 0;
            for v in 0..=max {
                let hex = format!("{v:0w$x}", w = digits as usize);
                let got = parse_rgb(&format!("rgb:{hex}/{hex}/{hex}")).unwrap().0;
                assert_eq!(got, scaled(v, digits), "{hex}");
                assert!(got >= last, "{hex} scaled below the value before it");
                last = got;
            }
            let zeros = "0".repeat(digits as usize);
            let fs = "f".repeat(digits as usize);
            assert_eq!(
                parse_rgb(&format!("rgb:{zeros}/{fs}/{fs}")),
                Some((0, 255, 255))
            );
        }
    }

    #[test]
    fn repeated_digits_mean_the_same_colour_at_every_width() {
        for v in 0..=255u32 {
            let two = format!("{v:02x}");
            let four = format!("{two}{two}");
            let spec = |c: &str| parse_rgb(&format!("rgb:{c}/{c}/{c}"));
            assert_eq!(spec(&two), Some((v as u8, v as u8, v as u8)));
            assert_eq!(spec(&four), spec(&two), "{four}");
        }
        for v in 0..16u32 {
            let one = format!("{v:x}");
            assert_eq!(
                parse_rgb(&format!("rgb:{one}/{one}/{one}")),
                parse_rgb(&format!("rgb:{one}{one}/{one}{one}/{one}{one}"))
            );
        }
    }

    #[test]
    fn channels_of_different_widths_scale_independently() {
        assert_eq!(parse_rgb("rgb:f/80/0000"), Some((255, 128, 0)));
        assert_eq!(parse_rgb("rgb:FFFF/8/abc"), Some((255, 136, 171)));
    }

    #[test]
    fn incomplete_or_empty_channels_are_rejected() {
        for spec in [
            "rgb:",
            "rgb:ff",
            "rgb:ff/ff",
            "rgb:ff/ff/",
            "rgb:ff//ff",
            "rgb://",
            "rgb:ff/ff/fffff",
            "rgb:ff/ff/ff ",
            "rgb:-1/00/00",
            " rgb:ff/ff/ff",
            "",
        ] {
            assert_eq!(parse_rgb(spec), None, "{spec:?}");
        }
    }

    #[test]
    fn other_osc_replies_are_not_mistaken_for_ours() {
        let buf = b"\x1b]4;11;rgb:ffff/0000/0000\x1b\\\x1b]110;rgb:ff/ff/ff\x07\x1b]111;rgb:0/0/0\x07\x1b[?62c";
        assert_eq!(parse(buf), Probed::default());
    }

    /// Without its terminator a reply may have been cut short by the
    /// timeout, and `rgb:1616/1818/1` would read as a different colour.
    #[test]
    fn a_reply_without_its_terminator_is_not_trusted() {
        assert_eq!(parse(b"\x1b]11;rgb:1616/1818/1").bg, None);
        assert_eq!(parse(b"\x1b]11;rgb:1616/1818/1c1c").bg, None);
        assert_eq!(
            parse(b"\x1b]11;rgb:1616/1818/1c1c\x07").bg,
            Some((0x16, 0x18, 0x1c))
        );
    }

    #[test]
    fn a_reply_cut_short_says_what_will_end_it() {
        assert_eq!(mid_reply(b"\x1b]11;rgb:16"), Some(Cut::Osc));
        assert_eq!(mid_reply(b"\x1b]11;rgb:16/18/1c\x1b"), Some(Cut::Osc));
        assert_eq!(
            mid_reply(b"\x1b]11;rgb:16/18/1c\x07\x1b[?62;2"),
            Some(Cut::Csi)
        );
    }

    #[test]
    fn a_reply_still_arriving_is_told_apart_from_a_finished_one() {
        for unfinished in [
            &b"\x1b"[..],
            b"\x1b]11;rgb:1616/18",
            b"\x1b]10;rgb:d4/d8/de\x1b\\\x1b]11;",
            b"\x1b]11;rgb:16/18/1c\x1b\\\x1b[?62;2",
            b"\x1b[",
        ] {
            assert!(
                mid_reply(unfinished).is_some(),
                "{:?}",
                String::from_utf8_lossy(unfinished)
            );
        }
        for finished in [
            &b""[..],
            b"noise",
            b"\x1b]11;rgb:16/18/1c\x1b\\",
            b"\x1b]11;rgb:16/18/1c\x07",
            b"\x1b]11;rgb:16/18/1c\x07\x1b[?62;22c",
        ] {
            assert_eq!(
                mid_reply(finished),
                None,
                "{:?}",
                String::from_utf8_lossy(finished)
            );
        }
    }

    #[test]
    fn a_da2_reply_is_not_taken_for_da1() {
        assert!(!has_da1(b"\x1b[>1;10;0c"));
        assert!(!has_da1(b"\x1b[62;22c"), "no `?`");
        assert!(!has_da1(b"\x1b[?62;22x"));
        assert!(!has_da1(b"\x1b[?6 2c"));
        assert!(
            has_da1(b"\x1b[?\x1b[?1;2c"),
            "a false start before the real one"
        );
    }

    #[test]
    fn replies_split_across_reads_are_seen_once_joined() {
        let whole = b"\x1b]10;rgb:d4d4/d8d8/dede\x07\x1b]11;rgb:1616/1818/1c1c\x07\x1b[?62;22c";
        let mut buf = Vec::new();
        for chunk in whole.chunks(5) {
            assert!(!has_da1(&buf));
            buf.extend_from_slice(chunk);
        }
        assert!(has_da1(&buf));
        assert_eq!(
            parse(&buf),
            Probed {
                fg: Some((0xd4, 0xd8, 0xde)),
                bg: Some((0x16, 0x18, 0x1c))
            }
        );
    }

    #[test]
    fn invalid_utf8_around_a_reply_does_not_hide_it() {
        let buf = b"\xff\xfe\xc3\x1b]11;rgb:00/80/ff\x1b\\\xe2\x82";
        assert_eq!(parse(buf).bg, Some((0, 0x80, 0xff)));
    }
}
