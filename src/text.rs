//! HN's comment HTML, turned into paragraphs of styled runs and then wrapped
//! into lines. Wrapping happens here rather than in a ratatui `Paragraph` so
//! the renderer knows exactly which screen cell every link lands on.
//!
//! HN serves a small, regular subset: `<p>`, `<a href>`, `<i>`, `<pre><code>`
//! and a handful of entities. That is all this handles.

use crate::util::width;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Italic,
    Code,
    /// Index into `Body::links`.
    Link(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default)]
pub struct Para {
    pub runs: Vec<Run>,
    /// Preformatted: keep line breaks, never reflow.
    pub pre: bool,
}

impl Para {
    /// HN's quoting convention: a paragraph that starts with `>`.
    pub fn is_quote(&self) -> bool {
        self.runs
            .iter()
            .find(|r| !r.text.trim().is_empty())
            .is_some_and(|r| r.text.trim_start().starts_with('>'))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Body {
    pub paras: Vec<Para>,
    pub links: Vec<String>,
}

impl Body {
    pub fn plain(text: &str) -> Self {
        Body {
            paras: vec![Para {
                runs: vec![Run {
                    text: text.to_string(),
                    kind: Kind::Plain,
                }],
                pre: false,
            }],
            links: Vec::new(),
        }
    }
}

pub fn parse(html: &str) -> Body {
    let mut body = Body::default();
    let mut para = Para::default();
    let mut italic = 0u32;
    let mut pre = false;
    let mut link: Option<usize> = None;

    let mut rest = html;
    while !rest.is_empty() {
        if let Some(inner) = rest.strip_prefix('<')
            && let Some(end) = inner.find('>')
        {
            let tag = &inner[..end];
            rest = &inner[end + 1..];
            let (closing, tag) = match tag.strip_prefix('/') {
                Some(t) => (true, t),
                None => (false, tag),
            };
            let name = tag
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_ascii_lowercase();
            match (closing, name.as_str()) {
                (_, "p") | (false, "br") => flush(&mut body, &mut para, pre),
                (false, "pre") => {
                    flush(&mut body, &mut para, pre);
                    pre = true;
                    para.pre = true;
                }
                (true, "pre") => {
                    flush(&mut body, &mut para, pre);
                    pre = false;
                    para.pre = false;
                }
                (false, "i" | "em") => italic += 1,
                (true, "i" | "em") => italic = italic.saturating_sub(1),
                (false, "a") => {
                    link = attr(tag, "href").map(|href| {
                        body.links.push(decode(href));
                        body.links.len() - 1
                    });
                }
                (true, "a") => link = None,
                _ => {}
            }
            continue;
        }

        // Text up to the next tag. A stray `<` with no `>` is just text.
        let next = if let Some(after) = rest.strip_prefix('<') {
            1 + after.find('<').unwrap_or(after.len())
        } else {
            rest.find('<').unwrap_or(rest.len())
        };
        let mut text = decode(&rest[..next]);
        rest = &rest[next..];
        if !pre {
            text = text.replace('\n', " ");
        }
        // Control characters are dropped when drawn; keep only the line
        // breaks and tabs a code block needs.
        text.retain(|c| !c.is_control() || (pre && matches!(c, '\n' | '\t')));
        let kind = match link {
            Some(i) => Kind::Link(i),
            None if pre => Kind::Code,
            None if italic > 0 => Kind::Italic,
            None => Kind::Plain,
        };
        match para.runs.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(&text),
            _ => para.runs.push(Run { text, kind }),
        }
    }
    flush(&mut body, &mut para, pre);
    body
}

fn flush(body: &mut Body, para: &mut Para, pre: bool) {
    let mut done = std::mem::replace(
        para,
        Para {
            runs: Vec::new(),
            pre,
        },
    );
    if done.pre
        && let Some(last) = done.runs.last_mut()
    {
        let trimmed = last.text.trim_end().len();
        last.text.truncate(trimmed);
    }
    if done.runs.iter().any(|r| !r.text.trim().is_empty()) {
        body.paras.push(done);
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let len = tag[start..].find('"')?;
    Some(&tag[start..start + len])
}

pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // Entities are short; a `;` far away belongs to something else.
        let semi = rest.bytes().take(12).position(|b| b == b';');
        let ch = semi.and_then(|end| {
            let ent = &rest[1..end];
            match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    // Digits only: `from_str_radix` would take a leading `+`.
                    .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| {
                        ent.strip_prefix('#')
                            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|d| d.parse().ok())
                    })
                    .and_then(char::from_u32),
            }
        });
        match (ch, semi) {
            (Some(c), Some(end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A piece of a wrapped line in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wrapped {
    pub segs: Vec<Seg>,
    pub quote: bool,
}

/// Lines of at most `width` columns, a blank line between paragraphs.
pub fn wrap(body: &Body, width: usize) -> Vec<Wrapped> {
    let width = width.max(8);
    let mut out = Vec::new();
    for (i, para) in body.paras.iter().enumerate() {
        if i > 0 {
            out.push(Wrapped::default());
        }
        if para.pre {
            let text: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            for line in text.lines() {
                for chunk in split_cols(&expand_tabs(line), width) {
                    out.push(Wrapped {
                        segs: vec![Seg {
                            text: chunk,
                            kind: Kind::Code,
                        }],
                        quote: false,
                    });
                }
            }
        } else {
            wrap_para(para, width, &mut out);
        }
    }
    out
}

fn wrap_para(para: &Para, width: usize, out: &mut Vec<Wrapped>) {
    let quote = para.is_quote();

    // Words, each with its style and whether whitespace came before it. A
    // style change mid-word (a link followed by a full stop) splits the word
    // but keeps the pieces glued.
    let mut atoms: Vec<(String, Kind, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_kind = Kind::Plain;
    let mut cur_space = false;
    let mut space = false;
    for run in &para.runs {
        for ch in run.text.chars() {
            if ch.is_whitespace() {
                if !cur.is_empty() {
                    atoms.push((std::mem::take(&mut cur), cur_kind, cur_space));
                }
                space = true;
                continue;
            }
            if !cur.is_empty() && cur_kind != run.kind {
                atoms.push((std::mem::take(&mut cur), cur_kind, cur_space));
            }
            if cur.is_empty() {
                cur_kind = run.kind;
                cur_space = space;
                space = false;
            }
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        atoms.push((cur, cur_kind, cur_space));
    }

    let mut line = Wrapped {
        segs: Vec::new(),
        quote,
    };
    let mut used = 0;
    for (text, kind, spaced) in atoms {
        let w = width_of(&text);
        let gap = usize::from(used > 0 && spaced);
        if used + gap + w <= width {
            if gap == 1 {
                // A space inside a link stays part of the link.
                let space_kind = match line.segs.last() {
                    Some(s) if s.kind == kind => kind,
                    _ => Kind::Plain,
                };
                push(&mut line, " ", space_kind);
            }
            push(&mut line, &text, kind);
            used += gap + w;
            continue;
        }
        if used > 0 {
            out.push(std::mem::replace(
                &mut line,
                Wrapped {
                    segs: Vec::new(),
                    quote,
                },
            ));
        }
        // Longer than a whole line (usually a URL): break it by columns.
        let mut chunks = split_cols(&text, width);
        let last = chunks.pop().unwrap_or_default();
        for chunk in chunks {
            push(&mut line, &chunk, kind);
            out.push(std::mem::replace(
                &mut line,
                Wrapped {
                    segs: Vec::new(),
                    quote,
                },
            ));
        }
        used = width_of(&last);
        push(&mut line, &last, kind);
    }
    if !line.segs.is_empty() {
        out.push(line);
    }
}

fn push(line: &mut Wrapped, text: &str, kind: Kind) {
    match line.segs.last_mut() {
        Some(last) if last.kind == kind => last.text.push_str(text),
        _ => line.segs.push(Seg {
            text: text.to_string(),
            kind,
        }),
    }
}

fn width_of(s: &str) -> usize {
    width(s)
}

/// Tabs to spaces, at stops every eight columns.
fn expand_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\t') {
        return line.into();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut col = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            let n = 8 - col % 8;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push_str(g);
            col += width(g);
        }
    }
    out.into()
}

/// Split into pieces of at most `cols` columns, never inside a grapheme.
fn split_cols(s: &str, cols: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = width(g);
        if used + w > cols && used > 0 {
            out.push(String::new());
            used = 0;
        }
        if let Some(last) = out.last_mut() {
            last.push_str(g);
        }
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(w: &Wrapped) -> String {
        w.segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn paragraphs_entities_and_links() {
        let b = parse(
            "It&#x27;s &quot;fine&quot; &amp; good<p>See <a href=\"https:&#x2F;&#x2F;x.io&#x2F;a\" rel=\"nofollow\">x.io</a>. <i>Really</i>",
        );
        assert_eq!(b.paras.len(), 2);
        assert_eq!(b.paras[0].runs[0].text, "It's \"fine\" & good");
        assert_eq!(b.links, vec!["https://x.io/a".to_string()]);
        let kinds: Vec<Kind> = b.paras[1].runs.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Plain, Kind::Link(0), Kind::Plain, Kind::Italic]
        );
    }

    #[test]
    fn pre_blocks_keep_their_lines() {
        let b = parse("look:<pre><code>  fn main() {\n      x();\n  }\n</code></pre>after");
        assert_eq!(b.paras.len(), 3);
        assert!(b.paras[1].pre && !b.paras[2].pre);
        let lines = wrap(&b, 40);
        let code: Vec<String> = lines
            .iter()
            .filter(|l| l.segs.first().is_some_and(|s| s.kind == Kind::Code))
            .map(plain)
            .collect();
        assert_eq!(code, vec!["  fn main() {", "      x();", "  }"]);
    }

    #[test]
    fn quotes_are_detected_by_their_leading_marker() {
        let b = parse("&gt; you said this<p>and I reply");
        assert!(b.paras[0].is_quote());
        assert!(!b.paras[1].is_quote());
    }

    #[test]
    fn wrapping_respects_width_and_keeps_punctuation_glued() {
        let b = parse("one two three four five six <a href=\"u\">link</a>, done");
        for w in [8, 11, 20, 60] {
            let lines = wrap(&b, w);
            assert!(lines.iter().all(|l| width(&plain(l)) <= w), "width {w}");
            let joined: Vec<String> = lines.iter().map(plain).collect();
            assert_eq!(
                joined.join(" "),
                "one two three four five six link, done",
                "width {w}"
            );
        }
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken_by_columns() {
        let url = "https://example.com/a/very/long/path/that/never/ends";
        let b = parse(&format!("see <a href=\"{url}\">{url}</a>"));
        let lines = wrap(&b, 16);
        assert!(lines.iter().all(|l| width(&plain(l)) <= 16));
        let rejoined: String = lines[1..].iter().map(plain).collect();
        assert_eq!(rejoined, url);
        assert!(lines[1..].iter().all(|l| l.segs[0].kind == Kind::Link(0)));
    }

    #[test]
    fn control_characters_go_and_tabs_become_spaces() {
        let b = parse("a&#27;[2Jb\u{7}<pre><code>\tx\r\nab\ty</code></pre>");
        assert_eq!(b.paras[0].runs[0].text, "a[2Jb");
        let lines: Vec<String> = wrap(&b, 40).iter().map(plain).collect();
        assert_eq!(lines[2..], ["        x", "ab      y"]);
    }

    #[test]
    fn broken_markup_does_not_panic() {
        for s in [
            "<",
            "a < b",
            "<a href=\"x\">unclosed",
            "&",
            "&#xZZ;",
            "&amp",
            "é&#233;ü",
        ] {
            let _ = wrap(&parse(s), 10);
        }
        assert_eq!(decode("é&#233;ü&amp"), "ééü&amp");
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use crate::util::char_width;
    use proptest::prelude::*;

    /// Columns of the first thing drawn: pieces are cut by grapheme.
    fn first_col(s: &str) -> usize {
        s.graphemes(true).next().map_or(0, width)
    }
    use proptest::sample::select;

    /// Characters whose string width is the sum of their character widths in
    /// any order. Emoji and script ligatures break that; they get their own
    /// tests below.
    const CHARS: &[char] = &[
        'a', 'b', 'e', 'x', 'Z', '0', '9', '.', ',', '-', '/', '>', '<', '&', ';', '#', '"', 'é',
        'ß', 'Ω', '…', '中', '字', 'あ', '한', '！', '😀', '🦀', '\u{301}', '\u{308}', '\u{200B}',
        '\u{AD}', ' ', ' ', ' ', '\n', '\t', '\u{A0}', '\u{3000}', '\u{0}', '\u{7}', '\u{1b}',
        '\r', '\u{7f}', '\u{85}', '\u{9c}',
    ];

    const TAGS: &[&str] = &[
        "<p>",
        "</p>",
        "<P>",
        "<p/>",
        "<i>",
        "</i>",
        "<I>",
        "<em>",
        "</em>",
        "<pre><code>",
        "</code></pre>",
        "<PRE>",
        "</pre>",
        "<br>",
        "<br/>",
        "<br />",
        "<a>",
        "</a>",
        "<a name=\"n\">",
        "<A HREF=\"u\">",
        "<span class=\"c\">",
        "</span>",
        "<",
        ">",
        "</",
        "<>",
        "<!-- c -->",
    ];

    const ENTITIES: &[&str] = &[
        "&amp;",
        "&lt;",
        "&gt;",
        "&quot;",
        "&apos;",
        "&nbsp;",
        "&#x27;",
        "&#x2F;",
        "&#X41;",
        "&#233;",
        "&#0;",
        "&#27;",
        "&#x9;",
        "&#10;",
        "&#x85;",
        "&#x301;",
        "&#x3000;",
        "&#x1F600;",
        "&#xD800;",
        "&#x110000;",
        "&#4294967296;",
        "&#99999999999;",
        "&#;",
        "&#x;",
        "&bogus;",
        "&",
        "&amp",
        "&#",
    ];

    fn chars_from(pool: &'static [char], max: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(select(pool), 0..max).prop_map(|v| v.into_iter().collect())
    }

    fn html() -> impl Strategy<Value = String> {
        let fragment = prop_oneof![
            4 => chars_from(CHARS, 12),
            3 => "[a-z]{1,14}",
            3 => select(TAGS).prop_map(String::from),
            1 => "[a-z:/.?=#&;]{0,24}".prop_map(|h| format!("<a href=\"{h}\" rel=\"nofollow\">")),
            2 => select(ENTITIES).prop_map(String::from),
        ];
        prop::collection::vec(fragment, 0..40).prop_map(|v| v.concat())
    }

    /// Columns a line takes on screen: the renderer measures segment by segment.
    fn cols(l: &Wrapped) -> usize {
        l.segs.iter().map(|s| width(&s.text)).sum()
    }

    fn text_of(l: &Wrapped) -> String {
        l.segs.iter().map(|s| s.text.as_str()).collect()
    }

    fn para_text(p: &Para) -> String {
        p.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Paragraph `i` on its own, so its lines can be told apart from the
    /// blank separators (a code block can hold blank lines of its own).
    fn only(body: &Body, i: usize) -> Body {
        Body {
            paras: vec![body.paras[i].clone()],
            links: body.links.clone(),
        }
    }

    /// What `parse` should make of markup-free text.
    fn as_prose(s: &str) -> String {
        s.replace('\n', " ")
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }

    /// Escapes every character in one of the forms HN (or anyone) might use,
    /// cycling through `how`. `&` and `<` are never left raw.
    fn escape(s: &str, how: &[u8]) -> String {
        let mut out = String::new();
        for (c, h) in s.chars().zip(how.iter().cycle()) {
            match (h % 5, c) {
                (_, '&') if h % 2 == 0 => out.push_str("&amp;"),
                (0, '<') => out.push_str("&lt;"),
                (0, '>') => out.push_str("&gt;"),
                (0, '"') => out.push_str("&quot;"),
                (0, '\'') => out.push_str("&apos;"),
                (1, c) if c != '&' && c != '<' => out.push(c),
                (2, c) | (0 | 1, c) => out.push_str(&format!("&#{};", c as u32)),
                (3, c) => out.push_str(&format!("&#x{:x};", c as u32)),
                (_, c) => out.push_str(&format!("&#X{:04X};", c as u32)),
            }
        }
        out
    }

    proptest! {
        #[test]
        fn parse_and_wrap_never_panic_on_arbitrary_strings(s in any::<String>(), w in 0usize..200) {
            let _ = wrap(&parse(&s), w);
        }

        #[test]
        fn parse_and_wrap_never_panic_on_html_like_input(s in html(), w in 0usize..200) {
            let _ = wrap(&parse(&s), w);
        }

        #[test]
        fn wrapped_lines_never_exceed_the_width(s in html(), w in 8usize..100) {
            for l in wrap(&parse(&s), w) {
                prop_assert!(cols(&l) <= w, "{} cols > {w}: {l:?}", cols(&l));
            }
        }

        #[test]
        fn widths_below_eight_wrap_as_eight(s in html(), w in 0usize..8) {
            let b = parse(&s);
            prop_assert_eq!(wrap(&b, w), wrap(&b, 8));
        }

        #[test]
        fn no_control_character_reaches_a_segment(
            s in prop_oneof![any::<String>(), html()],
            w in 0usize..120,
        ) {
            for l in wrap(&parse(&s), w) {
                for seg in &l.segs {
                    prop_assert!(!seg.text.contains(char::is_control), "{seg:?}");
                }
            }
        }

        #[test]
        fn parsed_runs_hold_only_the_line_breaks_and_tabs_code_needs(s in html()) {
            for p in parse(&s).paras {
                for r in &p.runs {
                    let bad = |c: char| c.is_control() && !(p.pre && matches!(c, '\n' | '\t'));
                    prop_assert!(!r.text.contains(bad), "{r:?} in pre={}", p.pre);
                }
            }
        }

        #[test]
        fn link_kinds_always_index_into_the_link_list(s in html(), w in 8usize..60) {
            let b = parse(&s);
            let ok = |k: Kind| !matches!(k, Kind::Link(i) if i >= b.links.len());
            for p in &b.paras {
                prop_assert!(p.runs.iter().all(|r| ok(r.kind)));
            }
            for l in wrap(&b, w) {
                prop_assert!(l.segs.iter().all(|s| ok(s.kind)));
            }
        }

        #[test]
        fn parse_keeps_only_visible_paragraphs_with_merged_runs(s in html()) {
            for p in parse(&s).paras {
                prop_assert!(p.runs.iter().any(|r| !r.text.trim().is_empty()), "{p:?}");
                prop_assert!(p.runs.windows(2).all(|w| w[0].kind != w[1].kind), "{p:?}");
                if p.pre {
                    let last = &p.runs.last().unwrap().text;
                    prop_assert_eq!(last.trim_end(), last.as_str());
                } else {
                    prop_assert!(p.runs.iter().all(|r| !r.text.contains('\n')));
                }
            }
        }

        #[test]
        fn prose_wraps_greedily_keeping_every_word_in_order(s in html(), w in 8usize..60) {
            let b = parse(&s);
            for i in (0..b.paras.len()).filter(|&i| !b.paras[i].pre) {
                // Each piece of unbroken, single-style text, keyed by where it
                // starts among the non-space characters: what wrap places whole.
                let mut pieces = std::collections::HashMap::new();
                let (mut n, mut start, mut prev) = (0, 0, None);
                for (c, kind) in b.paras[i].runs.iter().flat_map(|r| r.text.chars().map(move |c| (c, r.kind))) {
                    if c.is_whitespace() {
                        prev = None;
                        continue;
                    }
                    if prev != Some(kind) {
                        start = n;
                        pieces.insert(start, 0);
                    }
                    *pieces.get_mut(&start).unwrap() += char_width(c);
                    n += 1;
                    prev = Some(kind);
                }
                // A piece with no width (a lone combining mark) takes no room,
                // so the gap after it is not kept; harmless, and not this test.
                if pieces.values().any(|&pw| pw == 0) {
                    continue;
                }
                let want = para_text(&b.paras[i]).split_whitespace().collect::<Vec<_>>().join(" ");
                let (mut at, mut seen) = (0, 0);
                let mut before: Option<usize> = None;
                for l in wrap(&only(&b, i), w) {
                    let got = text_of(&l);
                    prop_assert!(!got.is_empty() && !got.starts_with(' ') && !got.ends_with(' '), "{got:?}");
                    // A line break stands for either one space or a cut inside a word.
                    let gap = usize::from(want[at..].starts_with(' '));
                    at += gap;
                    prop_assert!(want[at..].starts_with(&got), "{got:?} is not next in {want:?}");
                    at += got.len();
                    // Greedy: a piece that opens a line did not fit on the last
                    // one, or fits no line at all; a line that opens mid-piece
                    // follows one filled by that piece.
                    if let Some(used) = before {
                        let first = first_col(&got);
                        match pieces.get(&seen) {
                            Some(&pw) => prop_assert!(used + gap + pw > w || pw > w, "{got:?} fits after {used} of {w} cols"),
                            None => prop_assert!(used + first > w, "cut early before {got:?}"),
                        }
                    }
                    seen += got.chars().filter(|c| !c.is_whitespace()).count();
                    before = Some(cols(&l));
                }
                prop_assert_eq!(at, want.len(), "lost the tail of {:?}", want);
            }
        }

        #[test]
        fn styles_stay_attached_to_their_characters_through_wrapping(s in html(), w in 8usize..60) {
            let b = parse(&s);
            for i in (0..b.paras.len()).filter(|&i| !b.paras[i].pre) {
                let styled = |text: &str, kind: Kind| -> Vec<(char, Kind)> {
                    text.chars().filter(|c| !c.is_whitespace()).map(|c| (c, kind)).collect()
                };
                let want: Vec<_> = b.paras[i].runs.iter().flat_map(|r| styled(&r.text, r.kind)).collect();
                let got: Vec<_> = wrap(&only(&b, i), w)
                    .iter()
                    .flat_map(|l| l.segs.iter().flat_map(|s| styled(&s.text, s.kind)))
                    .collect();
                prop_assert_eq!(got, want);
            }
        }

        #[test]
        fn paragraphs_wrap_independently_one_blank_line_apart(s in html(), w in 8usize..60) {
            let b = parse(&s);
            let mut want = Vec::new();
            for i in 0..b.paras.len() {
                if i > 0 {
                    want.push(Wrapped::default());
                }
                let lines = wrap(&only(&b, i), w);
                prop_assert!(!lines.is_empty(), "paragraph {i} vanished: {:?}", b.paras[i]);
                want.extend(lines);
            }
            prop_assert_eq!(wrap(&b, w), want);
        }

        #[test]
        fn every_line_of_a_quote_and_no_other_carries_the_flag(s in html(), w in 8usize..60) {
            let b = parse(&s);
            for (i, p) in b.paras.iter().enumerate() {
                let quote = !p.pre && p.is_quote();
                prop_assert!(wrap(&only(&b, i), w).iter().all(|l| l.quote == quote));
            }
        }

        #[test]
        fn a_paragraph_opening_with_a_marker_is_a_quote(lead in "[ \n]{0,3}", rest in chars_from(CHARS, 20)) {
            let b = parse(&format!("<p>{lead}&gt;{rest}"));
            prop_assert!(b.paras[0].is_quote());
            let not = parse(&format!("<p>{lead}x&gt;{rest}"));
            prop_assert!(!not.paras[0].is_quote());
        }

        #[test]
        fn markup_free_text_passes_through_parse_unchanged(
            s in chars_from(CHARS, 60).prop_map(|s| s.replace(['<', '&'], "x")),
        ) {
            let b = parse(&s);
            let want = as_prose(&s);
            prop_assert!(b.links.is_empty());
            if want.trim().is_empty() {
                prop_assert!(b.paras.is_empty());
            } else {
                prop_assert_eq!(b.paras.len(), 1);
                prop_assert!(!b.paras[0].pre);
                prop_assert_eq!(&b.paras[0].runs, &vec![Run { text: want, kind: Kind::Plain }]);
            }
        }

        #[test]
        fn escaped_text_survives_parse_verbatim(
            s in chars_from(CHARS, 60),
            how in prop::collection::vec(any::<u8>(), 1..8),
        ) {
            let b = parse(&escape(&s, &how));
            let got: String = b.paras.iter().map(para_text).collect();
            let want = as_prose(&s);
            prop_assert_eq!(got, if want.trim().is_empty() { String::new() } else { want });
        }

        #[test]
        fn decode_roundtrips_text_escaped_with_any_entity_form(
            s in any::<String>(),
            how in prop::collection::vec(any::<u8>(), 1..8),
        ) {
            prop_assert_eq!(decode(&escape(&s, &how)), s);
        }

        #[test]
        fn decode_never_panics_and_only_ever_shrinks(s in "[&#;xX0-9a-fA-Fmpltgquosnb\u{e9}\u{4e2d}]{0,40}") {
            prop_assert!(decode(&s).len() <= s.len());
        }

        #[test]
        fn links_are_collected_decoded_and_in_document_order(
            hrefs in prop::collection::vec("[^\"<>]{0,16}", 0..6),
        ) {
            let html: String = hrefs
                .iter()
                .enumerate()
                .map(|(i, h)| format!("<a href=\"{}\" rel=\"nofollow\">l{i}</a> ", h.replace('&', "&amp;")))
                .collect();
            let b = parse(&html);
            prop_assert_eq!(&b.links, &hrefs);
            for r in b.paras.iter().flat_map(|p| &p.runs) {
                if let Kind::Link(i) = r.kind {
                    prop_assert_eq!(&r.text, &format!("l{i}"));
                }
            }
        }

        #[test]
        fn code_blocks_keep_every_line_and_never_reflow(
            lines in prop::collection::vec(chars_from(CODE, 30), 1..8),
        ) {
            let text = lines.join("\n");
            let want: Vec<&str> = text.trim_end().lines().collect();
            let wide = want.iter().map(|l| width(l)).max().unwrap_or(0).max(8);
            let got = wrap(&parse(&format!("x<pre><code>{text}</code></pre>")), wide);
            if text.trim().is_empty() {
                prop_assert_eq!(got.len(), 1, "an empty block leaves only the prose");
                return Ok(());
            }
            prop_assert_eq!(got[1].segs.len(), 0, "a blank line after the prose");
            let code = &got[2..];
            prop_assert!(code.iter().all(|l| !l.quote && l.segs.len() == 1 && l.segs[0].kind == Kind::Code));
            prop_assert_eq!(code.iter().map(text_of).collect::<Vec<_>>(), want);
        }

        #[test]
        fn a_long_code_line_is_cut_into_full_width_pieces(
            line in chars_from(CODE, 80).prop_filter("visible", |l| !l.trim().is_empty()),
            w in 8usize..30,
        ) {
            let got = wrap(&parse(&format!("<pre><code>{line}</code></pre>")), w);
            let pieces: Vec<String> = got.iter().map(text_of).collect();
            prop_assert_eq!(pieces.concat(), line.trim_end());
            for (i, p) in pieces.iter().enumerate() {
                prop_assert!(width(p) <= w, "{p:?} wider than {w}");
                if let Some(next) = pieces.get(i + 1) {
                    let first = first_col(next);
                    prop_assert!(width(p) + first > w, "{p:?} cut early before {next:?}");
                }
            }
        }

        #[test]
        // Two columns and up: `wrap` never asks for less than eight, and below
        // two a wide character cannot fit at all.
        fn split_cols_pieces_fit_rejoin_and_are_cut_greedily(s in chars_from(CODE, 60), w in 2usize..40) {
            let pieces = split_cols(&s, w);
            prop_assert_eq!(pieces.concat(), s.clone());
            if s.is_empty() {
                prop_assert_eq!(pieces, vec![String::new()]);
                return Ok(());
            }
            for (i, p) in pieces.iter().enumerate() {
                let first = first_col(p);
                prop_assert!(first > 0 || i == 0, "a zero-width mark was split from its base: {pieces:?}");
                prop_assert!(width(p) <= w, "{p:?} wider than {w}");
                if let Some(next) = pieces.get(i + 1) {
                    let next_first = first_col(next);
                    prop_assert!(width(p) + next_first > w, "{p:?} cut early before {next:?}");
                }
            }
        }

        #[test]
        fn tabs_expand_to_the_next_eight_column_stop(line in chars_from(TABBED, 40)) {
            let out = expand_tabs(&line);
            prop_assert!(!out.contains('\t'));
            for (i, c) in line.char_indices() {
                let before = expand_tabs(&line[..i]);
                prop_assert!(out.starts_with(&*before), "expansion depends only on what precedes");
                if c == '\t' {
                    let (from, to) = (width(&before), width(&expand_tabs(&line[..=i])));
                    prop_assert!(to % 8 == 0 && to > from && to - from <= 8, "tab at col {from} went to {to}");
                }
            }
            let kept = |s: &str| s.chars().filter(|c| !matches!(c, ' ' | '\t')).collect::<String>();
            prop_assert_eq!(kept(&out), kept(&line));
        }
    }

    /// Code-block content: no markup, no control characters but tab.
    const CODE: &[char] = &[
        'a', 'b', 'x', '0', '{', '}', '(', ')', ';', '=', '.', '中', '字', '😀', 'é', '\u{301}',
        '\u{200B}', ' ', ' ', ' ', '\u{A0}',
    ];

    const TABBED: &[char] = &['\t', '\t', '\t', 'a', 'b', ' ', '中', '😀', '\u{301}', 'é'];

    #[test]
    fn uppercase_tag_names_are_recognised() {
        let b = parse("a<P><I>b</I>c<PRE><CODE>d</CODE></PRE><BR>e");
        let kinds: Vec<Vec<Kind>> = b
            .paras
            .iter()
            .map(|p| p.runs.iter().map(|r| r.kind).collect())
            .collect();
        assert_eq!(
            kinds,
            [
                vec![Kind::Plain],
                vec![Kind::Italic, Kind::Plain],
                vec![Kind::Code],
                vec![Kind::Plain],
            ]
        );
        assert!(b.paras[2].pre);
    }

    #[test]
    fn an_anchor_without_an_href_is_plain_text_and_adds_no_link() {
        let b = parse("<a>x</a> <a name=\"n\" title=\"t\">y</a>");
        assert!(b.links.is_empty());
        assert_eq!(
            b.paras[0].runs,
            [Run {
                text: "x y".into(),
                kind: Kind::Plain
            }]
        );
    }

    #[test]
    fn an_href_is_found_after_other_attributes() {
        let b = parse("<a rel=\"nofollow\" href=\"https://a.b/?q=1&amp;r=2\">x</a>");
        assert_eq!(b.links, ["https://a.b/?q=1&r=2"]);
        assert_eq!(b.paras[0].runs[0].kind, Kind::Link(0));
    }

    #[test]
    fn stray_closing_tags_do_not_cancel_later_opening_ones() {
        let b = parse("</i></i></em></a><i>x</i>y");
        let kinds: Vec<(&str, Kind)> = b.paras[0]
            .runs
            .iter()
            .map(|r| (r.text.as_str(), r.kind))
            .collect();
        assert_eq!(kinds, [("x", Kind::Italic), ("y", Kind::Plain)]);
    }

    #[test]
    fn nested_italics_stay_italic_until_the_outermost_closes() {
        let b = parse("<i>a<em>b</em>c</i>d");
        let kinds: Vec<(&str, Kind)> = b.paras[0]
            .runs
            .iter()
            .map(|r| (r.text.as_str(), r.kind))
            .collect();
        assert_eq!(kinds, [("abc", Kind::Italic), ("d", Kind::Plain)]);
    }

    #[test]
    fn a_second_anchor_takes_over_and_one_close_ends_the_link() {
        let b = parse("<a href=\"1\">a<a href=\"2\">b</a>c</a>d");
        assert_eq!(b.links, ["1", "2"]);
        let kinds: Vec<(&str, Kind)> = b.paras[0]
            .runs
            .iter()
            .map(|r| (r.text.as_str(), r.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("a", Kind::Link(0)),
                ("b", Kind::Link(1)),
                ("cd", Kind::Plain)
            ]
        );
    }

    #[test]
    fn an_unclosed_link_wins_over_italics_to_the_end() {
        let b = parse("<a href=\"u\">a <i>b</i><p>c");
        let kinds: Vec<Vec<Kind>> = b
            .paras
            .iter()
            .map(|p| p.runs.iter().map(|r| r.kind).collect())
            .collect();
        assert_eq!(kinds, [vec![Kind::Link(0)], vec![Kind::Link(0)]]);
    }

    #[test]
    fn an_unclosed_code_block_runs_to_the_end() {
        let b = parse("<pre><code>  x\n  y");
        let lines: Vec<String> = wrap(&b, 40).iter().map(text_of).collect();
        assert_eq!(lines, ["  x", "  y"]);
        assert!(b.paras[0].pre);
    }

    #[test]
    fn only_the_opening_of_a_paragraph_decides_whether_it_quotes() {
        assert!(parse("&gt; see <a href=\"u\">this</a> <i>now</i> ok").paras[0].is_quote());
        assert!(!parse("plain <i>&gt; not a quote</i>").paras[0].is_quote());
    }

    #[test]
    fn lone_angle_brackets_are_text() {
        for s in ["1 < 2", "a<", "x < y <z", "a > b", "<3 you"] {
            assert_eq!(para_text(&parse(s).paras[0]), s, "{s:?}");
        }
    }

    #[test]
    fn every_spelling_of_a_break_splits_paragraphs() {
        let b = parse("a<br>b<br/>c<br />d<BR>e<p/>f</p>g");
        let texts: Vec<String> = b.paras.iter().map(para_text).collect();
        assert_eq!(texts, ["a", "b", "c", "d", "e", "f", "g"]);
    }

    #[test]
    fn whitespace_only_paragraphs_and_code_blocks_are_dropped() {
        let b = parse("<p>  <p>\n<p>&nbsp;<i> </i><pre><code>  \n \t</code></pre><p>&#7;");
        assert!(b.paras.is_empty(), "{:?}", b.paras);
    }

    #[test]
    fn code_blocks_trim_trailing_blank_lines_but_keep_inner_ones_and_indentation() {
        let b = parse("<pre><code>  x\n\n    y\n\n\n</code></pre>");
        let lines: Vec<String> = wrap(&b, 40).iter().map(text_of).collect();
        assert_eq!(lines, ["  x", "", "    y"]);
    }

    #[test]
    fn entities_at_the_very_end_of_input_are_handled() {
        for (s, want) in [
            ("x&amp;", "x&"),
            ("x&amp", "x&amp"),
            ("x&", "x&"),
            ("x&#", "x&#"),
            ("x&#65;", "xA"),
            ("x&#x41", "x&#x41"),
            ("x&;", "x&;"),
        ] {
            assert_eq!(decode(s), want, "{s:?}");
        }
    }

    #[test]
    fn nul_and_escape_entities_decode_but_never_reach_the_screen() {
        assert_eq!(decode("&#0;&#27;&#x7f;"), "\0\x1b\x7f");
        let b = parse("a&#0;b&#x1b;[2Jc&#x85;d");
        assert_eq!(para_text(&b.paras[0]), "ab[2Jcd");
    }

    #[test]
    fn surrogate_out_of_range_and_huge_numeric_entities_stay_verbatim() {
        for s in [
            "&#xD800;",
            "&#xdfff;",
            "&#55296;",
            "&#x110000;",
            "&#1114112;",
            "&#4294967295;",
            "&#4294967296;",
            "&#99999999999;",
            "&#;",
            "&#x;",
            "&#xg;",
            "&#-1;",
            "&bogus;",
        ] {
            assert_eq!(decode(s), s);
        }
    }

    #[test]
    fn the_highest_code_point_decodes_in_both_bases() {
        assert_eq!(
            decode("&#x10FFFF;&#1114111;&#X10ffff;"),
            "\u{10FFFF}".repeat(3)
        );
    }

    #[test]
    fn entities_are_only_recognised_within_a_short_window() {
        assert_eq!(decode("&#x00000041;"), "A");
        assert_eq!(decode("&#x000000041;"), "&#x000000041;");
    }

    #[test]
    fn entities_are_decoded_once_not_recursively() {
        assert_eq!(decode("&amp;lt;"), "&lt;");
        assert_eq!(decode("&&amp;"), "&&");
        assert_eq!(decode("&#38;#65;"), "&#65;");
        assert_eq!(decode("&amp;amp;"), "&amp;");
    }

    #[test]
    fn markup_inside_a_word_keeps_it_glued() {
        let lines = wrap(&parse("<i>foo</i>bar baz"), 8);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].segs,
            [
                Seg {
                    text: "foo".into(),
                    kind: Kind::Italic
                },
                Seg {
                    text: "bar".into(),
                    kind: Kind::Plain
                },
            ]
        );
        assert_eq!(text_of(&lines[1]), "baz");
    }

    #[test]
    fn a_space_inside_a_link_is_part_of_the_link_but_one_before_it_is_not() {
        let lines = wrap(&parse("see <a href=\"u\">two words</a> ok"), 40);
        assert_eq!(
            lines[0].segs,
            [
                Seg {
                    text: "see ".into(),
                    kind: Kind::Plain
                },
                Seg {
                    text: "two words".into(),
                    kind: Kind::Link(0)
                },
                Seg {
                    text: " ok".into(),
                    kind: Kind::Plain
                },
            ]
        );
    }

    #[test]
    fn wide_characters_are_never_split_and_never_overflow() {
        let han = "中".repeat(20);
        let lines: Vec<String> = wrap(&parse(&han), 9).iter().map(text_of).collect();
        assert_eq!(lines.concat(), han);
        assert!(
            lines
                .iter()
                .all(|l| width(l) == 8 || l == lines.last().unwrap())
        );
        let code = wrap(&parse(&format!("<pre><code>{han}</code></pre>")), 9);
        assert!(code.iter().all(|l| cols(l) <= 9));
        assert_eq!(code.iter().map(text_of).collect::<String>(), han);
    }

    #[test]
    fn a_wide_character_counts_two_columns_toward_the_next_tab_stop() {
        assert_eq!(expand_tabs("中\tx"), "中      x");
        assert_eq!(expand_tabs("1234567\tx"), "1234567 x");
        assert_eq!(expand_tabs("12345678\tx"), "12345678        x");
        assert_eq!(expand_tabs("e\u{301}\tx"), "e\u{301}       x");
        assert_eq!(expand_tabs("\t\t"), " ".repeat(16));
    }

    // Only the first run is inspected, so a leading space (or an empty run
    // left by a control character parse dropped) before `<i>` hides the
    // marker, though the paragraph still reads as `> quoted` on screen.
    #[test]
    fn a_quote_marker_after_leading_whitespace_and_a_tag_still_marks_a_quote() {
        assert!(parse("<p> <i>&gt; quoted</i> and plain").paras[0].is_quote());
        assert!(parse("<p>&#1;<i>&gt; quoted</i>").paras[0].is_quote());
    }

    // `❤` is one column on its own and two with VS16 after it. `wrap` measures
    // words with the string width but cuts them with per-character widths, so
    // a cut piece can be wider than the line.
    const EMOJI: &[&str] = &[
        "\u{2764}\u{FE0F}",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F1EE}\u{1F1F8}",
        "\u{1F980}",
        "a",
        "\u{4E2D}",
    ];

    proptest! {
        /// A code line of well-formed emoji sequences is cut into pieces
        /// that fit, between graphemes, never inside one.
        #[test]
        fn emoji_sequences_are_cut_between_graphemes_and_fit(
            parts in prop::collection::vec(select(EMOJI), 1..40),
            w in 8usize..30,
        ) {
            let line: String = parts.concat();
            let lines = wrap(&parse(&format!("<pre><code>{line}</code></pre>")), w);
            prop_assert!(lines.iter().all(|l| cols(l) <= w));
            let pieces: Vec<String> = lines.iter().map(text_of).collect();
            prop_assert_eq!(pieces.concat(), line.clone());
            let cut: Vec<&str> = pieces.iter().flat_map(|p| p.graphemes(true)).collect();
            let whole: Vec<&str> = line.graphemes(true).collect();
            prop_assert_eq!(cut, whole);
        }
    }

    #[test]
    fn emoji_presentation_sequences_never_push_a_line_past_the_width() {
        let hearts = "\u{2764}\u{FE0F}".repeat(6);
        for html in [hearts.clone(), format!("<pre><code>{hearts}</code></pre>")] {
            for l in wrap(&parse(&html), 8) {
                assert!(cols(&l) <= 8, "{} cols: {l:?}", cols(&l));
            }
        }
    }
}
