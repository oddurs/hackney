//! Rendering. Every frame also records the screen regions it drew into
//! (`app.hits`) so the mouse handler can turn a click back into an action.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Hits, Target, Thread};
use crate::hn::{Feed, Story};
use crate::text::{self, Body, Kind, Wrapped};
use crate::theme::Theme;
use crate::util::{cell, now_secs, pad, plural, rel_time, right, truncate, width};

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Replies deeper than this stop indenting, so a long chain keeps some room.
const MAX_INDENT: usize = 10;

/// Columns a reply's text keeps however deep it is: in a narrow pane the
/// indentation gives way first.
const MIN_BODY: usize = 30;

// Breakpoints, by viewport width.

/// From this width the list and the comments sit side by side; below it
/// they stack.
const SIDE_BY_SIDE: u16 = 120;

/// The list's share of a side-by-side layout, bounded so it always has room
/// for comment counts and never turns into a field of blank space.
const LIST_SHARE: u16 = 45;
const LIST_MIN: u16 = 56;
const LIST_MAX: u16 = 100;

/// The widest the comments get, for reading. Beyond it the pane is left
/// blank beside the text, or the text is centred when zoomed.
const MEASURE: u16 = 100;

/// Where field values begin in the detail pane: margin plus a ten-column label.
const VALUE_COL: usize = 12;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.hits = Hits::default();
    app.layout.roots.clear();

    let t = app.theme;
    f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), area);

    // Three rows of chrome, as in rigor: a nav line, the feed tabs, and a rail
    // that underlines the active tab and separates chrome from content.
    let [nav, subnav, rail, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    let split = body_layout(app, body);
    draw_nav(f, nav, app);
    draw_subnav(f, subnav, rail, app, &split);
    draw_body(f, &split, app);
    draw_footer(f, footer, app);

    if app.show_help {
        draw_help(f, area, app);
    }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// Cut a composed line to `width` columns. Nothing in the detail pane may
/// wrap on its own: that would put the recorded hitboxes out of step with
/// what was drawn.
fn clip(line: Line<'static>, max: usize) -> Line<'static> {
    let style = line.style;
    let mut used = 0usize;
    let mut out = Vec::new();
    for span in line.spans {
        if used >= max {
            break;
        }
        let w = width(&span.content);
        if used + w <= max {
            used += w;
            out.push(span);
        } else {
            out.push(Span::styled(
                truncate(&span.content, max - used),
                span.style,
            ));
            break;
        }
    }
    Line::from(out).style(style)
}

/// ` hackney │ Top stories  30 of 500                              ● 12s ago `
fn draw_nav(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let muted = Style::new().fg(t.muted);

    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            "hackney",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::new().fg(t.border)),
        Span::styled(
            app.feed.heading(),
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        ),
    ];
    let count = (!app.ids.is_empty()).then(|| {
        // Once every story has been asked for, the count is what is shown:
        // dead and missing ones never arrive, and "2 of 6" would promise them.
        let all = app.requested >= app.ids.len() && !app.loading_more;
        if app.feed == Feed::Search || all || app.stories.len() >= app.ids.len() {
            format!("  {}", plural(app.stories.len() as u32, "story", "stories"))
        } else {
            format!("  {} of {}", app.stories.len(), app.ids.len())
        }
    });

    let now = now_secs();
    let dot = |c: Color| Span::styled("●", Style::new().fg(c));
    let mut right = if app.busy() {
        vec![
            Span::styled(
                SPINNER[app.spinner % SPINNER.len()],
                Style::new().fg(t.accent),
            ),
            Span::styled(" loading", muted),
        ]
    } else if app.error.is_some() {
        vec![dot(t.failure), Span::styled(" failed", muted)]
    } else if app.last_fetch > 0 {
        let age = now - app.last_fetch;
        vec![
            dot(if age > 600 { t.pending } else { t.success }),
            Span::styled(format!(" {} ago", rel_time(app.last_fetch, now)), muted),
        ]
    } else {
        Vec::new()
    };
    right.push(Span::raw(" "));

    // The status, then the count, each whole or not at all; the query gets
    // whatever room is left. Half of "40 stories" reads as something else.
    let w = area.width as usize;
    let mut room = w.saturating_sub(width_of(&left) + 1);
    if width_of(&right) <= room {
        room -= width_of(&right);
    } else {
        right.clear();
    }
    let count = count.filter(|c| width(c) <= room);
    room -= count.as_deref().map_or(0, width);
    if app.feed == Feed::Search && room >= 6 {
        left.push(Span::styled(
            format!("  “{}”", truncate(&app.query, room - 4)),
            Style::new().fg(t.fg),
        ));
    }
    if let Some(count) = count {
        left.push(Span::styled(count, muted));
    }
    let gap = w.saturating_sub(width_of(&left) + width_of(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    f.render_widget(Paragraph::new(Line::from(left)), area);
}

/// ` Top   New   Best   Ask   Show   Jobs   ⌕ rust`
/// `─━━━━─────────────────────────┬─────────────────`
fn draw_subnav(f: &mut Frame, area: Rect, rail: Rect, app: &mut App, split: &BodySplit) {
    let t = app.theme;
    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;
    let mut underline: Option<(u16, u16)> = None;

    let labels: Vec<(Feed, String)> = app
        .tabs()
        .into_iter()
        .map(|feed| {
            let label = match feed {
                Feed::Search => format!("⌕ {}", truncate(&app.query, 18)),
                _ => feed.title().to_string(),
            };
            (feed, label)
        })
        .collect();
    // Tabs close up to one space apart when the usual two don't fit.
    let roomy: usize = labels.iter().map(|(_, l)| width(l) + 3).sum();
    let gap: u16 = if roomy > area.width as usize { 1 } else { 2 };

    for (feed, label) in labels {
        let active = feed == app.feed;
        let style = if active {
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(t.fg)
        };
        let w = (width(&label) + 2) as u16;
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" ".repeat(gap as usize)));
        if x < area.right() {
            let visible = w.min(area.right() - x);
            // The target covers the tab and its stretch of rail: two rows tall.
            app.hits.zones.push((
                Rect::new(x, area.y, visible, area.height + rail.height),
                Target::Tab(feed),
            ));
            if active {
                underline = Some((x + 1, (w - 2).min(visible.saturating_sub(1))));
            }
        }
        x = x.saturating_add(w + gap - 1);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // When the list is longer than its pane, say where you are in it.
    let marker = split
        .list
        .filter(|l| app.stories.len() > l.height as usize)
        .map(|_| {
            (
                split.divider_x.unwrap_or(rail.right()),
                format!(" {}/{} ", app.selected + 1, app.stories.len()),
            )
        });
    f.render_widget(
        Paragraph::new(rail_line(rail, underline, split.divider_x, marker, &t)),
        rail,
    );
}

/// The rule under the tabs: light everywhere, heavy and accented under the
/// active tab, and a `┬` where the detail pane's divider comes down to meet it.
fn rail_line(
    rail: Rect,
    underline: Option<(u16, u16)>,
    divider_x: Option<u16>,
    marker: Option<(u16, String)>,
    t: &Theme,
) -> Line<'static> {
    let light = Style::new().fg(t.border);
    let heavy = Style::new().fg(t.accent);

    let mut cells: Vec<(char, Style)> = (rail.x..rail.right()).map(|_| ('─', light)).collect();
    if let Some(dx) = divider_x
        && dx >= rail.x
        && dx < rail.right()
    {
        cells[(dx - rail.x) as usize] = ('┬', light);
    }
    let under = |cx: u16| underline.is_some_and(|(ux, uw)| cx >= ux && cx < ux + uw);
    if let Some((end_x, text)) = marker {
        let w = text.chars().count() as u16;
        let start = end_x.saturating_sub(1 + w).max(rail.x);
        // Whole or not at all: a marker half under the tab reads as a
        // different number.
        let fits = start + w <= rail.right() && !(start..start + w).any(under);
        if fits {
            for (i, ch) in text.chars().enumerate() {
                cells[(start - rail.x) as usize + i] = (ch, Style::new().fg(t.muted));
            }
        }
    }
    if let Some((ux, uw)) = underline {
        for cx in ux..(ux + uw).min(rail.right()) {
            cells[(cx - rail.x) as usize] = ('━', heavy);
        }
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = light;
    for (ch, st) in cells {
        if st != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = st;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

struct BodySplit {
    list: Option<Rect>,
    detail_outer: Rect,
    borders: Borders,
    divider_x: Option<u16>,
}

/// Side by side on a wide viewport, stacked on a narrower one, and the
/// comments alone when zoomed.
fn body_layout(app: &App, area: Rect) -> BodySplit {
    if app.zoom {
        return BodySplit {
            list: None,
            detail_outer: area,
            borders: Borders::NONE,
            divider_x: None,
        };
    }
    if area.width >= SIDE_BY_SIDE {
        let list = (area.width * LIST_SHARE / 100).clamp(LIST_MIN, LIST_MAX);
        let [l, d] =
            Layout::horizontal([Constraint::Length(list), Constraint::Fill(1)]).areas(area);
        BodySplit {
            list: Some(l),
            detail_outer: d,
            borders: Borders::LEFT,
            divider_x: Some(d.x),
        }
    } else {
        let [l, d] = Layout::vertical([Constraint::Percentage(40), Constraint::Min(4)]).areas(area);
        BodySplit {
            list: Some(l),
            detail_outer: d,
            borders: Borders::TOP,
            divider_x: None,
        }
    }
}

fn draw_body(f: &mut Frame, split: &BodySplit, app: &mut App) {
    let block = Block::new()
        .borders(split.borders)
        .border_style(Style::new().fg(app.theme.border));
    let mut detail = block.inner(split.detail_outer);
    f.render_widget(block, split.detail_outer);

    if let Some(list) = split.list {
        app.hits.list = list;
        draw_list(f, list, app);
    }
    app.hits.detail = detail;
    if detail.width > MEASURE {
        if app.zoom {
            detail.x += (detail.width - MEASURE) / 2;
        }
        detail.width = MEASURE;
    }
    draw_detail(f, detail, app);
}

// ------------------------------------------------------------------ list

fn draw_list(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let height = area.height as usize;
    app.clamp_scroll(height);

    if app.stories.is_empty() {
        let msg = if app.loading_feed {
            "loading…".to_string()
        } else if let Some(e) = &app.error {
            format!("✗ {e}")
        } else {
            "Nothing here.".to_string()
        };
        // Wrapped: the end of an error is usually the part that says why.
        let lines: Vec<Line> =
            text::wrap(&Body::plain(&msg), (area.width as usize).saturating_sub(4))
                .into_iter()
                .map(|l| {
                    let s: String = l.segs.into_iter().map(|s| s.text).collect();
                    Line::from(Span::styled(format!("  {s}"), Style::new().fg(t.muted)))
                })
                .collect();
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let now = now_secs();
    let w = area.width as usize;
    let end = (app.offset + height).min(app.stories.len());
    let mut lines = Vec::with_capacity(height);
    for i in app.offset..end {
        let y = area.y + (i - app.offset) as u16;
        app.hits
            .zones
            .push((Rect::new(area.x, y, area.width, 1), Target::Row(i)));
        let story = &app.stories[i];
        let read = app.read.contains(&story.id);
        lines.push(story_line(
            story,
            i + 1,
            i == app.selected,
            read,
            w,
            now,
            &t,
        ));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Dim on an unselected row, full foreground on the selected one, so the
/// whole row brightens as you move through the list.
fn dim(selected: bool, t: &Theme) -> Color {
    if selected { t.fg } else { t.muted }
}

/// `▎ 1  ▲ 439  ↳ 305  Shopify moves back to Native  shopify.engineering   fnthawar2    3h`
fn story_line(
    story: &Story,
    rank: usize,
    selected: bool,
    read: bool,
    w: usize,
    now: i64,
    t: &Theme,
) -> Line<'static> {
    let base = if selected {
        Style::new().bg(t.sel_bg)
    } else {
        Style::new()
    };
    let show_comments = w >= 56;
    let show_age = w >= 48;
    let show_author = w >= 96;

    let mut spans = vec![
        Span::styled(if selected { "▎" } else { " " }, base.fg(t.accent)),
        Span::styled(
            format!("{}  ", right(&rank.to_string(), 3)),
            base.fg(if selected { t.accent } else { t.muted }),
        ),
        Span::styled("▲ ", base.fg(t.accent)),
        Span::styled(
            format!("{}  ", right(&story.score.to_string(), 4)),
            if story.score >= 300 {
                base.fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                base.fg(dim(selected, t))
            },
        ),
    ];
    let mut left = 1 + 5 + 2 + 6;
    if show_comments {
        spans.push(Span::styled("↳ ", base.fg(t.muted)));
        spans.push(Span::styled(
            format!("{}  ", right(&story.comments.to_string(), 4)),
            base.fg(dim(selected, t)),
        ));
        left += 8;
    }

    let right_w = if show_author { 14 } else { 0 } + if show_age { 5 } else { 0 };
    let title_w = w.saturating_sub(left + right_w).max(8);

    // A gutter before the author column; the age is right-aligned and
    // brings its own.
    let title = truncate(
        &story.title,
        title_w.saturating_sub(usize::from(show_author)),
    );
    let mut rest = title_w.saturating_sub(width(&title));
    let title_style = match (selected, read) {
        (true, _) => base.fg(t.fg).add_modifier(Modifier::BOLD),
        (false, true) => base.fg(t.muted),
        (false, false) => base.fg(t.fg),
    };
    spans.push(Span::styled(title, title_style));
    // The domain goes in whole or not at all: half a hostname says nothing.
    if let Some(domain) = story.domain()
        && width(domain) + 3 <= rest
    {
        rest -= 2 + width(domain);
        spans.push(Span::styled(format!("  {domain}"), base.fg(t.muted)));
    }
    spans.push(Span::styled(" ".repeat(rest), base));

    if show_author {
        spans.push(Span::styled(cell(&story.by, 14), base.fg(dim(selected, t))));
    }
    if show_age {
        spans.push(Span::styled(
            format!("{} ", right(&rel_time(story.time, now), 4)),
            base.fg(dim(selected, t)),
        ));
    }
    Line::from(spans).style(base)
}

// ---------------------------------------------------------------- detail

/// The detail pane as a document: lines, the click targets on them, and
/// where each top-level comment starts.
#[derive(Default)]
struct Doc {
    lines: Vec<Line<'static>>,
    /// (line, x, width, target)
    targets: Vec<(usize, usize, usize, Target)>,
    roots: Vec<(usize, usize)>,
}

impl Doc {
    fn push(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(spans));
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }

    /// A target on the line about to be pushed.
    fn target(&mut self, x: usize, w: usize, target: Target) {
        self.targets.push((self.lines.len(), x, w, target));
    }

    /// Wrapped text at column `x`, each line prefixed by `prefix`, with a
    /// target over every link.
    fn text(&mut self, body: &Body, prefix: &[Span<'static>], x: usize, w: usize, t: &Theme) {
        for line in text::wrap(body, w) {
            let mut spans = prefix.to_vec();
            let mut cx = x;
            for seg in &line.segs {
                let sw = width(&seg.text);
                if let Kind::Link(i) = seg.kind
                    && let Some(url) = body.links.get(i)
                {
                    self.target(cx, sw, Target::Open(url.clone()));
                }
                spans.push(Span::styled(
                    seg.text.clone(),
                    seg_style(seg.kind, &line, t),
                ));
                cx += sw;
            }
            self.push(spans);
        }
    }
}

fn seg_style(kind: Kind, line: &Wrapped, t: &Theme) -> Style {
    match kind {
        Kind::Plain if line.quote => Style::new().fg(t.muted).add_modifier(Modifier::ITALIC),
        Kind::Plain => Style::new().fg(t.fg),
        Kind::Italic => Style::new().fg(t.fg).add_modifier(Modifier::ITALIC),
        Kind::Code => Style::new().fg(t.code),
        Kind::Link(_) => Style::new().fg(t.link).add_modifier(Modifier::UNDERLINED),
    }
}

fn field(k: &str, t: &Theme) -> Span<'static> {
    Span::styled(format!("  {}", pad(k, 10)), Style::new().fg(t.muted))
}

fn strip_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// The detail document as last built. Rebuilding means wrapping every
/// comment, so it is kept until the thread, its folds, the width or the
/// minute changes — scrolling only copies out the lines on screen.
#[derive(Default)]
pub struct Cache {
    /// (story, thread rev, width, minute)
    key: Option<(u64, u64, usize, i64)>,
    doc: Doc,
}

fn draw_detail(f: &mut Frame, area: Rect, app: &mut App) {
    if area.width < 12 || area.height < 2 {
        return;
    }
    let t = app.theme;
    let w = area.width as usize;
    let mut cache = std::mem::take(&mut app.doc);
    let Some(story) = app.selected_story() else {
        app.doc = cache;
        return;
    };
    match app.thread() {
        Some(thread) => {
            let key = (story.id, thread.rev, w, now_secs() / 60);
            if cache.key != Some(key) {
                cache.doc = document(story, Ok(thread), w, &t, app.spinner);
                cache.key = Some(key);
            }
        }
        None => {
            let error = if app.pending.contains(&story.id) || app.settling() {
                None
            } else {
                app.thread_errors.get(&story.id).cloned()
            };
            cache.doc = document(story, Err(error), w, &t, app.spinner);
            cache.key = None;
        }
    }
    let doc = &cache.doc;

    let height = area.height as usize;
    app.detail_scroll = app
        .detail_scroll
        .min(doc.lines.len().saturating_sub(height));
    let scroll = app.detail_scroll;
    let first = doc.targets.partition_point(|(line, ..)| *line < scroll);
    for (line, x, tw, target) in &doc.targets[first..] {
        if *line >= scroll + height {
            break;
        }
        if *x < w {
            let rect = Rect::new(
                area.x + *x as u16,
                area.y + (line - scroll) as u16,
                (*tw).min(w - x) as u16,
                1,
            );
            app.hits.zones.push((rect, target.clone()));
        }
    }
    app.layout.roots.clone_from(&doc.roots);

    let end = (scroll + height).min(doc.lines.len());
    let lines: Vec<Line> = doc.lines[scroll..end]
        .iter()
        .map(|l| clip(l.clone(), w))
        .collect();
    f.render_widget(Paragraph::new(lines), area);
    app.doc = cache;
}

/// `state` is the loaded thread, or — while there isn't one — `None` for
/// loading and `Some(error)` for a failure.
fn document(
    story: &Story,
    state: Result<&Thread, Option<String>>,
    w: usize,
    t: &Theme,
    spinner: usize,
) -> Doc {
    let now = now_secs();
    let muted = Style::new().fg(t.muted);
    let mut doc = Doc::default();
    // A loaded thread carries the fresh score and count.
    let story = state.as_ref().map_or(story, |th| &th.story);

    let title = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
    for line in text::wrap(&Body::plain(&story.title), w.saturating_sub(2)) {
        let s: String = line.segs.into_iter().map(|s| s.text).collect();
        doc.target(1, width(&s), Target::Open(story.link()));
        doc.push(vec![Span::raw(" "), Span::styled(s, title)]);
    }

    let mut meta = vec![Span::raw(" ")];
    if let Some(d) = story.domain() {
        meta.push(Span::styled(d.to_string(), Style::new().fg(t.fg)));
        meta.push(Span::styled(" · ", muted));
    }
    meta.extend([
        Span::styled("▲ ", Style::new().fg(t.accent)),
        Span::styled(story.score.to_string(), Style::new().fg(t.fg)),
        Span::styled(" · by ", muted),
        Span::styled(story.by.clone(), Style::new().fg(t.fg)),
        Span::styled(format!(" · {} ago", rel_time(story.time, now)), muted),
    ]);
    doc.push(meta);
    doc.blank();

    let link = Style::new().fg(t.link).add_modifier(Modifier::UNDERLINED);
    let value_w = w.saturating_sub(VALUE_COL + 1);
    if let Some(url) = &story.url {
        let shown = truncate(strip_scheme(url), value_w);
        doc.target(VALUE_COL, width(&shown), Target::Open(url.clone()));
        doc.push(vec![field("article", t), Span::styled(shown, link)]);
    }
    let hn = story.hn_url();
    // Narrow, the id is the part worth keeping.
    let shown = if width(strip_scheme(&hn)) <= value_w {
        strip_scheme(&hn).to_string()
    } else {
        truncate(&format!("item?id={}", story.id), value_w)
    };
    doc.target(VALUE_COL, width(&shown), Target::Open(hn.clone()));
    doc.push(vec![field("thread", t), Span::styled(shown, link)]);

    let thread = match state {
        Ok(thread) => thread,
        Err(error) => {
            doc.blank();
            match error {
                Some(e) => doc.push(vec![Span::styled(
                    format!("  ✗ {e}"),
                    Style::new().fg(t.failure),
                )]),
                None => doc.push(vec![
                    Span::styled(
                        format!("  {} ", SPINNER[spinner % SPINNER.len()]),
                        Style::new().fg(t.accent),
                    ),
                    Span::styled("loading comments", muted),
                ]),
            }
            return doc;
        }
    };

    if let Some(body) = &thread.text {
        doc.blank();
        doc.text(body, &[Span::raw(" ")], 1, w.saturating_sub(2), t);
    }

    // `─ 305 comments ──────────`
    doc.blank();
    let label = if thread.nodes.is_empty() && story.comments > 0 {
        // Every one of them dead or deleted.
        "no comments to show".to_string()
    } else if thread.nodes.is_empty() {
        "no comments yet".to_string()
    } else {
        plural(
            story.comments.max(thread.nodes.len() as u32),
            "comment",
            "comments",
        )
    };
    let rule = w.saturating_sub(width(&label) + 5);
    doc.push(vec![
        Span::styled(" ─ ", Style::new().fg(t.border)),
        Span::styled(label, muted),
        Span::styled(format!(" {}", "─".repeat(rule)), Style::new().fg(t.border)),
    ]);

    // A column of margin, then one rail per level of nesting.
    let guide = Style::new().fg(t.border);
    let guides = |depth: usize| -> Vec<Span<'static>> {
        std::iter::once(Span::raw(" "))
            .chain((0..depth).map(|_| Span::styled("│ ", guide)))
            .collect()
    };
    let max_indent = (w.saturating_sub(MIN_BODY + 3) / 2).min(MAX_INDENT);
    let mut prev: Option<usize> = None;
    for i in thread.visible() {
        let node = &thread.nodes[i];
        let depth = node.depth.min(max_indent);
        // The spacer carries the guides both neighbours share, so a thread's
        // rail runs unbroken from a reply to the next one.
        doc.push(guides(prev.map_or(0, |p| p.min(depth))));
        prev = Some(depth);

        if node.depth == 0 {
            doc.roots.push((doc.lines.len(), i));
        }
        let indent = 1 + depth * 2;
        let op = node.by == story.by;
        let mut header = guides(depth);
        header.push(Span::styled(if node.folded { "▸ " } else { "▾ " }, muted));
        header.push(Span::styled(
            node.by.clone(),
            Style::new()
                .fg(if op { t.accent } else { t.fg })
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::styled(
            format!("  {}", rel_time(node.time, now)),
            muted,
        ));
        let replies = node.replies(i);
        if node.folded && replies > 0 {
            header.push(Span::styled(
                format!("  +{replies}"),
                Style::new().fg(t.accent),
            ));
        }
        doc.target(0, w, Target::Fold(i));
        doc.push(header);

        if !node.folded {
            let mut prefix = guides(depth);
            prefix.push(Span::raw("  "));
            let body_w = w.saturating_sub(indent + 3).max(8);
            doc.text(&node.body, &prefix, indent + 2, body_w, t);
        }
    }
    doc
}

// ---------------------------------------------------------------- footer

/// The footer is the command line: keys with a word each, no badges.
fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let muted = Style::new().fg(t.muted);

    let w = area.width as usize;
    if let Some(q) = &app.prompt {
        // The hint goes first, then the start of the query: what is being
        // typed stays in view.
        const HINT: &str = "     enter search · esc cancel";
        let room = w.saturating_sub(3);
        let hint = if width(q) + width(HINT) <= room {
            HINT
        } else {
            ""
        };
        let shown = if width(q) <= room {
            q.clone()
        } else {
            use unicode_segmentation::UnicodeSegmentation;
            let mut tail: Vec<&str> = Vec::new();
            let mut used = 1;
            for g in q.graphemes(true).rev() {
                used += width(g);
                if used > room {
                    break;
                }
                tail.push(g);
            }
            std::iter::once("…").chain(tail.into_iter().rev()).collect()
        };
        let line = Line::from(vec![
            Span::styled(" /", Style::new().fg(t.accent)),
            Span::styled(shown, Style::new().fg(t.fg)),
            Span::styled("▏", Style::new().fg(t.accent)),
            Span::styled(hint, muted),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some(e) = &app.error {
        const RETRY: &str = "  r retry ";
        let room = w.saturating_sub(3 + RETRY.len());
        let line = Line::from(vec![
            Span::styled(" ✗ ", Style::new().fg(t.failure)),
            Span::styled(truncate(e, room), Style::new().fg(t.failure)),
            Span::styled(RETRY, muted),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some((msg, at)) = &app.notice
        && now_secs() - at < 4
    {
        let line = Line::from(Span::styled(format!(" {msg}"), Style::new().fg(t.accent)));
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let key = |k: &str, d: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(k.to_string(), Style::new().fg(t.fg)),
            Span::styled(format!(" {d}   "), muted),
        ]
    };
    let tabs = app.tabs().len();
    let groups = [
        key(&format!("1–{tabs}"), "feed"),
        key("o", "open"),
        key("c", "discuss"),
        key("n/p", "thread"),
        key("x", "fold"),
        key("/", "search"),
        key("z", if app.zoom { "unzoom" } else { "zoom" }),
        key("r", "refresh"),
    ];
    let mut right = key("?", "help");
    right.extend([
        Span::styled("q", Style::new().fg(t.fg)),
        Span::styled(" quit ", muted),
    ]);

    let budget = w.saturating_sub(width_of(&right) + 1);
    let mut spans = vec![Span::raw(" ")];
    for group in groups {
        if width_of(&spans) + width_of(&group) > budget {
            break;
        }
        spans.extend(group);
    }
    let gap = w.saturating_sub(width_of(&spans) + width_of(&right));
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ------------------------------------------------------------------ help

const KEYS: &[(&str, &str)] = &[
    ("j / k, ↓ / ↑", "move through stories"),
    ("g / G", "first / last"),
    // `#` is the number of tabs there are.
    ("1–#, tab", "switch feed"),
    ("enter, o", "open the article"),
    ("c", "open the discussion on HN"),
    ("J / K, space / b", "scroll comments by line / page"),
    ("n / p", "next / previous thread"),
    ("x", "fold the thread you're reading"),
    ("z", "comments full width"),
    ("/", "search Hacker News"),
    ("r", "refresh"),
    ("?", "close this help"),
    ("q", "quit"),
];

const MOUSE: &[(&str, &str)] = &[
    ("click a tab", "switch feed"),
    ("click a story", "read its comments"),
    ("double-click a story", "open the article"),
    ("click a comment's name", "fold its replies"),
    ("click a link", "open it in the browser"),
    ("scroll wheel", "scroll the list or the comments"),
];

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    // Recede everything behind the modal so it reads as a layer on top.
    let buf = f.buffer_mut();
    for pos in area.positions() {
        if let Some(c) = buf.cell_mut(pos) {
            c.modifier.insert(Modifier::DIM);
        }
    }

    let w = 64u16.min(area.width.saturating_sub(4));
    let h = ((KEYS.len() + MOUSE.len() + 4) as u16).min(area.height);
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border))
        .title(Span::styled(
            " keys ",
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::new().bg(t.bg));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Each table's key column as wide as its widest key, so a narrow modal
    // still leaves room for the descriptions.
    let tabs = app.tabs().len().to_string();
    let col = |table: &[(&str, &str)]| table.iter().map(|(k, _)| width(k)).max().unwrap_or(0) + 2;
    let row = |k: &str, d: &str, col: usize| {
        Line::from(vec![
            Span::styled(
                format!("  {}", pad(&k.replace('#', &tabs), col)),
                Style::new().fg(t.accent),
            ),
            Span::styled(d.to_string(), Style::new().fg(t.fg)),
        ])
    };
    let (keys, mouse) = (col(KEYS), col(MOUSE));
    let mut lines: Vec<Line> = KEYS.iter().map(|(k, d)| row(k, d, keys)).collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  mouse",
        Style::new().fg(t.muted).add_modifier(Modifier::BOLD),
    )));
    lines.extend(MOUSE.iter().map(|(k, d)| row(k, d, mouse)));
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use super::draw;
    use crate::app::tests::comment;
    use crate::app::{App, Target, Thread};
    use crate::hn::{Feed, Story};
    use crate::probe::Probed;
    use crate::theme::Theme;
    use crate::util::{now_secs, width};

    fn story(id: u64, title: &str, url: Option<&str>) -> Story {
        Story {
            id,
            title: title.into(),
            by: "pg".into(),
            score: 120 + id as u32,
            time: now_secs() - 3600 * id as i64,
            comments: 4,
            url: url.map(Into::into),
            ..Story::default()
        }
    }

    fn sample() -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.stories = vec![
            story(
                1,
                "Show HN: A terminal HN reader",
                Some("https://example.com/post"),
            ),
            story(2, "Ask HN: What are you working on?", None),
            story(
                3,
                "A very long title that goes on and on well past the width of any list pane",
                Some("https://www.long-domain-name.example.org/x"),
            ),
        ];
        let comments = vec![
            comment(
                "alice",
                "First paragraph with a <a href=\"https://a.example/1\">https://a.example/1</a> link.<p>Second paragraph.",
                0,
                vec![comment(
                    "pg",
                    "&gt; quoted<p>reply from the author",
                    1,
                    vec![comment("carol", "deep", 2, vec![])],
                )],
            ),
            comment("dave", "<pre><code>  let x = 1;\n</code></pre>", 0, vec![]),
        ];
        let s = a.stories[0].clone();
        a.threads.insert(1, Thread::new(s, comments));
        a
    }

    fn render(a: &mut App, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, a)).unwrap();
        term.backend().buffer().clone()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn wide_layout_shows_list_and_thread_side_by_side() {
        let mut a = sample();
        let buf = render(&mut a, 160, 40);
        let screen: Vec<String> = (0..40).map(|y| row(&buf, y)).collect();
        assert!(screen[0].contains("hackney │ Top stories"));
        assert!(screen[1].contains("Top") && screen[1].contains("Jobs"));
        assert!(screen[2].contains('┬'), "the rail meets the divider");
        assert!(screen[3].starts_with("▎  1  ▲  121"), "{}", screen[3]);
        assert!(screen[3].contains("example.com"));
        assert!(screen.iter().any(|l| l.contains("▾ alice")));
        assert!(screen.iter().any(|l| l.contains("│ ▾ pg")));
        assert!(screen.iter().any(|l| l.contains("│ │ ▾ carol")));
    }

    /// The whole point of recording targets while drawing: every one must sit
    /// on the thing it claims to be.
    #[test]
    fn click_targets_land_on_what_was_drawn() {
        let mut a = sample();
        let buf = render(&mut a, 160, 40);
        let mut folds = 0;
        let mut links = 0;
        for (r, target) in &a.hits.zones {
            let text: String = (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect();
            match target {
                Target::Fold(i) => {
                    let who = &a.thread().unwrap().nodes[*i].by;
                    assert!(text.contains(&format!("▾ {who}")), "fold {i} on `{text}`");
                    folds += 1;
                }
                Target::Open(url) if url.starts_with("https://a.example") => {
                    assert_eq!(text, "https://a.example/1");
                    let underlined = (r.x..r.right())
                        .all(|x| buf[(x, r.y)].modifier.contains(Modifier::UNDERLINED));
                    assert!(underlined, "link cells are underlined");
                    links += 1;
                }
                Target::Row(i) => assert!(text.contains(&(i + 1).to_string())),
                _ => {}
            }
        }
        assert_eq!(folds, 4);
        assert_eq!(links, 1);
    }

    #[test]
    fn folding_hides_replies_and_counts_them() {
        let mut a = sample();
        a.toggle_fold(0);
        let buf = render(&mut a, 160, 40);
        let screen: Vec<String> = (0..40).map(|y| row(&buf, y)).collect();
        assert!(
            screen
                .iter()
                .any(|l| l.contains("▸ alice") && l.contains("+2"))
        );
        assert!(!screen.iter().any(|l| l.contains("carol")));
        assert!(screen.iter().any(|l| l.contains("▾ dave")));
    }

    #[test]
    fn narrow_and_tiny_terminals_stack_or_degrade_without_panicking() {
        let mut a = sample();
        let buf = render(&mut a, 80, 30);
        let screen: Vec<String> = (0..30).map(|y| row(&buf, y)).collect();
        assert!(!screen[2].contains('┬'), "stacked: no side divider");
        assert!(
            screen
                .iter()
                .any(|l| l.contains("Show HN: A terminal HN reader"))
        );
        for (w, h) in [(20, 6), (40, 10), (1, 1)] {
            render(&mut a, w, h);
        }
        a.zoom = true;
        let buf = render(&mut a, 100, 30);
        assert!(!row(&buf, 3).contains("▲"), "zoom hides the list");
    }

    fn screen(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height).map(|y| row(buf, y)).collect()
    }

    #[test]
    fn the_nav_counts_what_is_shown_once_everything_was_asked_for() {
        let mut a = sample();
        // Six ids, all asked for; three dead or missing, so three came back.
        a.ids = vec![1, 2, 3, 4, 5, 6];
        a.requested = 6;
        assert!(row(&render(&mut a, 120, 20), 0).contains("  3 stories"));
        a.ids = (1..=100).collect();
        a.requested = 30;
        assert!(row(&render(&mut a, 120, 20), 0).contains("  3 of 100"));
    }

    #[test]
    fn side_by_side_the_list_is_bounded_and_keeps_its_counts() {
        for w in [120u16, 160, 240] {
            let mut a = sample();
            let buf = render(&mut a, w, 30);
            let s = screen(&buf);
            let divider = s[3].chars().position(|c| c == '│').expect("side by side");
            assert!((56..=100).contains(&divider), "{w}: list is {divider} wide");
            assert!(s[3].contains("↳    4"), "{w}: comment counts shown");
        }
        let buf = render(&mut sample(), 119, 30);
        assert!(!screen(&buf)[2].contains('┬'), "119 stacks");
    }

    #[test]
    fn comments_stop_at_a_readable_measure_and_centre_when_zoomed() {
        let mut a = sample();
        let buf = render(&mut a, 240, 30);
        let pane = a.hits.detail;
        for y in pane.y..pane.bottom() {
            for x in pane.x + 100..pane.right() {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "drawn past the measure at {x},{y}"
                );
            }
        }
        a.zoom = true;
        let buf = render(&mut a, 240, 30);
        let s = screen(&buf);
        let title = s
            .iter()
            .find(|l| l.contains("Show HN: A terminal"))
            .unwrap();
        let at = title.find("Show HN").unwrap();
        assert!((60..80).contains(&at), "centred: title at column {at}");
    }

    #[test]
    fn a_deep_chain_in_a_narrow_pane_gives_up_indentation_not_text() {
        let (mut a, _rx) = App::new(Theme::default());
        a.stories = vec![story(1, "Deep", None)];
        let mut chain = comment("last", "the deepest reply is still readable", 14, vec![]);
        for d in (0..14).rev() {
            chain = comment(&format!("u{d}"), "hi", d, vec![chain]);
        }
        a.threads
            .insert(1, Thread::new(a.stories[0].clone(), vec![chain]));
        a.detail_scroll = usize::MAX; // to the bottom
        let buf = render(&mut a, 50, 60);
        let text: String = screen(&buf).join("\n");
        for word in ["the", "deepest", "reply", "is", "still", "readable"] {
            assert!(text.contains(word), "lost `{word}`:\n{text}");
        }
    }

    #[test]
    fn tabs_close_up_when_they_do_not_fit() {
        let mut a = sample();
        a.query = "rust".into();
        let buf = render(&mut a, 44, 20);
        let tabs = row(&buf, 1);
        for label in ["Top", "New", "Best", "Ask", "Show", "Jobs", "⌕ rust"] {
            assert!(tabs.contains(label), "`{label}` missing from `{tabs}`");
        }
        let buf = render(&mut a, 120, 20);
        assert!(
            row(&buf, 1).contains("Top   New"),
            "roomy when there is room"
        );
    }

    #[test]
    fn a_large_thread_draws_quickly() {
        let (mut a, _rx) = App::new(Theme::default());
        a.stories = vec![story(1, "Big", None)];
        let para = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ";
        let body = format!("{}<p>{}", para.repeat(3), para.repeat(2));
        let comments = (0..600)
            .map(|i| {
                comment(
                    &format!("user{i}"),
                    &body,
                    0,
                    vec![comment("r", &body, 1, vec![comment("s", &body, 2, vec![])])],
                )
            })
            .collect();
        a.threads
            .insert(1, Thread::new(a.stories[0].clone(), comments));
        let start = std::time::Instant::now();
        render(&mut a, 180, 50);
        let first = start.elapsed();
        // Scrolling reuses the document; only the lines on screen are drawn.
        let start = std::time::Instant::now();
        for i in 0..20 {
            a.detail_scroll = i * 40;
            render(&mut a, 180, 50);
        }
        let per = start.elapsed() / 20;
        eprintln!("1800 comments: {first:?} to build, {per:?} per scrolled frame");
        assert!(per * 4 < first, "scrolling should not rebuild the document");
    }

    // ------------------------------------------------------------ snapshots

    /// The screen as text in an ASCII frame, so its full width shows. A wide
    /// glyph stands in for the cell it covers.
    fn framed(buf: &Buffer) -> String {
        let w = buf.area.width;
        let edge = format!("+{}+", "-".repeat(w as usize));
        let mut out = vec![edge.clone()];
        for y in 0..buf.area.height {
            let mut line = String::from("|");
            let mut x = 0;
            while x < w {
                let sym = buf[(x, y)].symbol();
                line.push_str(sym);
                x += (width(sym) as u16).max(1);
            }
            line.push('|');
            out.push(line);
        }
        out.push(edge);
        out.join("\n")
    }

    fn snap(name: &str, a: &mut App, w: u16, h: u16) -> Buffer {
        let buf = render(a, w, h);
        insta::assert_snapshot!(name, framed(&buf));
        buf
    }

    /// As just fetched: the nav line has a count and an age.
    fn fetched(mut a: App) -> App {
        a.ids = a.stories.iter().map(|s| s.id).collect();
        // As a real load leaves it: every story on screen was asked for.
        a.requested = a.ids.len();
        a.last_fetch = now_secs() - 90;
        a
    }

    const TITLES: [(&str, Option<&str>); 12] = [
        (
            "Shopify moves back to native",
            Some("https://shopify.engineering/native"),
        ),
        (
            "Ask HN: How do you keep up with papers in your field?",
            None,
        ),
        (
            "The unreasonable effectiveness of plain text",
            Some("https://www.example.org/plain-text"),
        ),
        (
            "Show HN: I built a tiny Forth that runs in a boot sector",
            Some("https://github.com/someone/sectorforth"),
        ),
        (
            "SQLite is not a toy database",
            Some("https://antonz.org/sqlite-is-not-a-toy-database/"),
        ),
        (
            "Why the moon has no atmosphere to speak of (2019)",
            Some("https://www.scientificamerican.com/article/moon"),
        ),
        (
            "Rust 1.90 released",
            Some("https://blog.rust-lang.org/2026/09/rust-1.90.html"),
        ),
        (
            "Launch HN: Acme (YC S26) – Invoices that pay themselves",
            None,
        ),
        (
            "A deep dive into how terminals render text, one cell at a time",
            Some("https://jvns.ca/blog/terminal-rendering"),
        ),
        (
            "Pg_search: full-text search in Postgres without Elasticsearch",
            Some("https://paradedb.com/blog"),
        ),
        (
            "Stripe is hiring infrastructure engineers (Remote)",
            Some("https://stripe.com/jobs"),
        ),
        (
            "I miss the old web",
            Some("https://www.theverge.com/old-web"),
        ),
    ];
    const AUTHORS: [&str; 7] = [
        "fnthawar2",
        "pg",
        "dang",
        "tptacek",
        "patio11",
        "throwaway_account_2026",
        "jl",
    ];
    const SCORES: [u32; 9] = [439, 87, 1204, 33, 301, 7, 4321, 150, 299];
    const COUNTS: [u32; 8] = [305, 12, 988, 0, 41, 1, 2345, 77];

    /// `n` stories of a front page, with a small thread on every one.
    fn front_page(n: usize) -> App {
        let (mut a, _rx) = App::new(Theme::default());
        let now = now_secs();
        a.stories = (0..n)
            .map(|i| {
                let (title, url) = TITLES[i % TITLES.len()];
                Story {
                    id: 100 + i as u64,
                    title: title.into(),
                    by: AUTHORS[i % AUTHORS.len()].into(),
                    score: SCORES[i % SCORES.len()],
                    comments: COUNTS[i % COUNTS.len()],
                    time: now - 1800 * (i as i64 + 1),
                    url: url.map(Into::into),
                    ..Story::default()
                }
            })
            .collect();
        for s in &a.stories {
            let comments = vec![
                comment(
                    "alice",
                    "Great write-up.",
                    0,
                    vec![comment("bob", "Agreed.", 1, vec![])],
                ),
                comment("carol", "Has anyone benchmarked this?", 0, vec![]),
            ];
            a.threads.insert(s.id, Thread::new(s.clone(), comments));
        }
        fetched(a)
    }

    /// Story 2 of the sample, an Ask HN, selected with its text and replies.
    fn ask_hn() -> App {
        let mut a = fetched(sample());
        a.stories[1].text = Some(
            "Curious what side projects people are hacking on this month.<p>I'll start: a terminal reader for HN, see <a href=\"https://github.com/example/hackney\">https://github.com/example/hackney</a>. Feedback welcome!".into(),
        );
        let comments = vec![
            comment(
                "alice",
                "A garden planner that knows my frost dates.",
                0,
                vec![],
            ),
            comment(
                "pg",
                "Replying to my own thread: essays, as ever.",
                0,
                vec![],
            ),
        ];
        a.threads
            .insert(2, Thread::new(a.stories[1].clone(), comments));
        a.selected = 1;
        a
    }

    /// One comment with everything HN can put in one, and a deep chain of
    /// replies under it.
    fn rich() -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.stories = vec![story(
            1,
            "Show HN: A terminal HN reader",
            Some("https://example.com/post"),
        )];
        let mut chain = comment(
            "zed",
            "The deepest reply in the chain still keeps thirty columns of text however far down it sits.",
            7,
            vec![],
        );
        for d in (1..7).rev() {
            chain = comment(
                &format!("user{d}"),
                &format!("Reply at depth {d}."),
                d,
                vec![chain],
            );
        }
        let body = "Two things. <i>First</i>, a quote:<p>&gt; The quick brown fox jumps over the lazy dog, and keeps on running well past the edge of the pane.<p>Second, some code:<p><pre><code>fn main() {\n    println!(\"a line of code longer than the pane is wide, by some way\");\n}\n</code></pre><p>Source: <a href=\"https://github.com/example/repository/blob/main/src/deeply/nested/module/file.rs#L120\">https://github.com/example/repository/blob/main/src/deeply/nested/module/file.rs#L120</a>.";
        let comments = vec![
            comment("tptacek", body, 0, vec![chain]),
            comment("pg", "Thanks, all.", 0, vec![]),
        ];
        a.threads
            .insert(1, Thread::new(a.stories[0].clone(), comments));
        fetched(a)
    }

    /// Titles in scripts and symbols that are two columns a glyph.
    fn wide_titles() -> App {
        let (mut a, _rx) = App::new(Theme::default());
        let now = now_secs();
        let titles = [
            (
                "日本語のタイトル：Rustで書かれた端末向けハッカーニュースリーダーの設計について",
                Some("https://zenn.dev/article"),
            ),
            (
                "🦀 Rust 2026 edition is out 🎉",
                Some("https://blog.rust-lang.org/"),
            ),
            (
                "Café naïve résumé: Zürich's Straße über alles",
                Some("https://www.nzz.ch/x"),
            ),
            ("한국어 제목도 제대로 보여야 합니다", None),
            (
                "I ❤️ plain text ❤️ and so should you ❤️ really, truly, deeply, madly",
                Some("https://example.com/"),
            ),
        ];
        a.stories = titles
            .iter()
            .enumerate()
            .map(|(i, (title, url))| Story {
                id: i as u64 + 1,
                title: (*title).into(),
                by: "kentaro".into(),
                score: 200 + i as u32 * 97,
                comments: 3 + i as u32 * 40,
                time: now - 3600 * (i as i64 + 1),
                url: url.map(Into::into),
                ..Story::default()
            })
            .collect();
        let comments = vec![comment(
            "kentaro",
            "端末の幅に合わせて折り返されるべき長い日本語のコメントです。句読点も含めて、正しく表示されるでしょうか。",
            0,
            vec![comment("emoji_fan", "🦀🦀🦀 ship it 🚀", 1, vec![])],
        )];
        a.threads
            .insert(1, Thread::new(a.stories[0].clone(), comments));
        fetched(a)
    }

    #[test]
    fn the_sample_app_renders_at_every_breakpoint() {
        for (w, h) in [
            (30, 10),
            (44, 16),
            (60, 24),
            (80, 24),
            (119, 30),
            (120, 30),
            (160, 40),
            (240, 40),
        ] {
            snap(&format!("layout_{w:03}x{h}"), &mut fetched(sample()), w, h);
        }
    }

    #[test]
    fn a_full_front_page_renders_stacked_and_side_by_side() {
        snap("front_page_080x24", &mut front_page(30), 80, 24);
        snap("front_page_160x40", &mut front_page(30), 160, 40);
    }

    #[test]
    fn a_feed_still_loading_shows_a_spinner_and_an_empty_list() {
        let (mut a, _rx) = App::new(Theme::default());
        a.loading_feed = true;
        a.spinner = 2;
        snap("feed_loading_080x24", &mut a, 80, 24);
        snap("feed_loading_160x40", &mut a, 160, 40);
    }

    #[test]
    fn a_feed_that_failed_says_why_in_the_list_and_the_footer() {
        let (mut a, _rx) = App::new(Theme::default());
        a.error = Some(
            "https://hacker-news.firebaseio.com/v0/topstories.json: Connection Failed: Connect error: connection refused (os error 61)".into(),
        );
        snap("feed_error_080x24", &mut a, 80, 24);
    }

    #[test]
    fn a_search_with_no_results_says_nothing_here() {
        let (mut a, _rx) = App::new(Theme::default());
        a.feed = Feed::Search;
        a.query = "zzqxj".into();
        a.last_fetch = now_secs() - 90;
        snap("empty_search_080x24", &mut a, 80, 24);
    }

    #[test]
    fn comments_on_their_way_show_a_spinner_in_the_pane_and_the_nav() {
        let mut a = fetched(sample());
        a.selected = 1;
        a.pending.insert(2);
        a.spinner = 5;
        snap("comments_loading_120x30", &mut a, 120, 30);
    }

    #[test]
    fn comments_that_failed_to_load_show_the_error_in_the_pane() {
        let mut a = fetched(sample());
        a.selected = 1;
        a.thread_errors.insert(
            2,
            "https://hn.algolia.com/api/v1/items/2: status code 503".into(),
        );
        snap("comments_error_120x30", &mut a, 120, 30);
    }

    #[test]
    fn a_thread_with_no_comments_says_so() {
        let mut a = fetched(sample());
        a.selected = 2;
        a.threads
            .insert(3, Thread::new(a.stories[2].clone(), vec![]));
        snap("no_comments_120x30", &mut a, 120, 30);
    }

    #[test]
    fn an_ask_hn_post_shows_its_text_above_the_comments() {
        snap("ask_hn_120x30", &mut ask_hn(), 120, 30);
        snap("ask_hn_080x30", &mut ask_hn(), 80, 30);
    }

    #[test]
    fn a_folded_thread_shows_how_many_replies_it_hides() {
        let mut a = fetched(sample());
        a.toggle_fold(0);
        snap("folded_120x30", &mut a, 120, 30);
    }

    #[test]
    fn zoom_gives_the_comments_the_whole_body_and_centres_them_when_wide() {
        let mut a = fetched(sample());
        a.zoom = true;
        snap("zoomed_100x30", &mut a, 100, 30);
        snap("zoomed_240x40", &mut a, 240, 40);
    }

    #[test]
    fn help_floats_over_the_screen_at_any_size() {
        let mut a = fetched(sample());
        a.show_help = true;
        snap("help_080x24", &mut a, 80, 24);
        snap("help_100x30", &mut a, 100, 30);
        snap("help_040x12", &mut a, 40, 12);
    }

    #[test]
    fn the_search_prompt_shows_what_is_typed_in_the_footer() {
        let mut a = fetched(sample());
        a.prompt = Some("rust async".into());
        snap("search_prompt_080x24", &mut a, 80, 24);
        a.prompt = Some("why is async rust so much harder to learn than go".into());
        snap("search_prompt_long_044x16", &mut a, 44, 16);
    }

    #[test]
    fn the_search_tab_truncates_a_long_query_and_the_nav_shows_it_whole() {
        let mut a = front_page(40);
        a.feed = Feed::Search;
        a.query = "why is async rust so much harder to learn than go".into();
        snap("search_tab_080x24", &mut a, 80, 24);
        snap("search_tab_120x30", &mut a, 120, 30);
        let mut a = front_page(40);
        a.feed = Feed::Search;
        a.query = "rust".into();
        a.selected = 2;
        snap("search_tab_short_044x16", &mut a, 44, 16);
    }

    #[test]
    fn a_notice_takes_the_footer_for_a_few_seconds() {
        let mut a = fetched(sample());
        a.note("opened https://example.com/post");
        snap("notice_080x24", &mut a, 80, 24);
    }

    #[test]
    fn a_failure_with_stories_on_screen_takes_the_footer() {
        let mut a = fetched(sample());
        a.error = Some("could not load more stories: timed out".into());
        snap("footer_error_120x30", &mut a, 120, 30);
    }

    #[test]
    fn a_scrolled_list_marks_its_position_on_the_rail() {
        let mut a = front_page(50);
        a.selected = 30;
        snap("list_scrolled_080x24", &mut a, 80, 24);
        let mut a = front_page(50);
        a.selected = 44;
        a.loading_more = true;
        snap("list_scrolled_160x40", &mut a, 160, 40);
    }

    #[test]
    fn wide_titles_keep_the_list_columns_in_line() {
        snap("wide_titles_080x24", &mut wide_titles(), 80, 24);
        snap("wide_titles_160x40", &mut wide_titles(), 160, 40);
    }

    #[test]
    fn a_rich_comment_wraps_code_quotes_and_links_in_a_narrow_pane() {
        let mut a = rich();
        a.zoom = true;
        snap("rich_comment_zoomed_044x64", &mut a, 44, 64);
        snap("rich_comment_060x40", &mut rich(), 60, 40);
    }

    #[test]
    fn jumping_to_the_next_thread_brings_it_to_the_top_of_the_pane() {
        let mut a = rich();
        render(&mut a, 120, 24);
        a.jump(true);
        snap("jumped_to_first_thread_120x24", &mut a, 120, 24);
        // The last thread can only come as far up as the end of the document.
        a.jump(true);
        snap("jumped_past_the_last_thread_120x24", &mut a, 120, 24);
    }

    // ---------------------------------------------------------------- bugs
    //
    // These fail on purpose: each pins a defect found reviewing the snapshots.

    #[test]
    fn a_title_with_emoji_keeps_the_age_column_in_line() {
        let mut a = wide_titles();
        let buf = render(&mut a, 80, 24);
        let list = a.hits.list;
        for i in 0..a.stories.len() as u16 {
            let y = list.y + i;
            let age: String = (list.right() - 3..list.right() - 1)
                .map(|x| buf[(x, y)].symbol())
                .collect();
            assert_eq!(
                age,
                format!("{}h", i + 1),
                "row {}:\n{}",
                i + 1,
                row(&buf, y)
            );
        }
    }

    #[test]
    fn help_fits_every_line_on_an_80_by_24_terminal() {
        let mut a = fetched(sample());
        a.show_help = true;
        let text = screen(&render(&mut a, 80, 24)).join("\n");
        assert!(text.contains("scroll wheel"), "last line cut:\n{text}");
    }

    #[test]
    fn the_rail_marker_is_shown_whole_or_not_at_all() {
        let mut a = front_page(40);
        a.feed = Feed::Search;
        a.query = "rust".into();
        a.selected = 2;
        let rail = row(&render(&mut a, 44, 16), 2);
        let digits = rail.chars().any(|c| c.is_ascii_digit());
        assert!(
            rail.contains(" 3/40 ") || !digits,
            "half a marker: `{rail}`"
        );
    }

    #[test]
    fn the_search_prompt_keeps_the_cursor_in_view() {
        let mut a = fetched(sample());
        a.prompt = Some("why is async rust so much harder to learn than go".into());
        let footer = row(&render(&mut a, 44, 16), 15);
        assert!(footer.contains("go▏"), "typing off screen: `{footer}`");
    }

    // -------------------------------------------------------------- styles

    /// Runs of one style along a row, one to a line.
    fn runs(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
        let mut out: Vec<(String, ratatui::style::Style)> = Vec::new();
        let mut x = from;
        while x < to {
            let cell = &buf[(x, y)];
            let style = cell.style();
            match out.last_mut() {
                Some((text, s)) if *s == style => text.push_str(cell.symbol()),
                _ => out.push((cell.symbol().to_string(), style)),
            }
            x += (width(cell.symbol()) as u16).max(1);
        }
        out.iter()
            .map(|(text, s)| {
                format!(
                    "{text:?} fg={:?} bg={:?} {:?}",
                    s.fg.unwrap_or(Color::Reset),
                    s.bg.unwrap_or(Color::Reset),
                    s.add_modifier
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where `needle` starts on row `y`, by column.
    fn find(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
        let first = needle.chars().next()?;
        (0..buf.area.width).find(|&x| {
            buf[(x, y)].symbol().starts_with(first) && {
                let text: String = (x..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                text.starts_with(needle)
            }
        })
    }

    fn cells(buf: &Buffer, y: u16, x: u16, n: usize) -> Vec<&ratatui::buffer::Cell> {
        (x..x + n as u16).map(|x| &buf[(x, y)]).collect()
    }

    fn row_with(buf: &Buffer, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| row(buf, y).contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not on screen"))
    }

    fn probed() -> Theme {
        Theme::resolve_with(
            Probed {
                fg: Some((220, 220, 210)),
                bg: Some((24, 24, 28)),
            },
            false,
        )
    }

    #[test]
    fn read_and_unread_rows_differ_only_in_how_the_title_is_drawn() {
        let mut a = fetched(sample());
        a.theme = probed();
        a.read.extend([1, 2]);
        let buf = render(&mut a, 120, 30);
        let list = a.hits.list;
        let rows: Vec<String> = (0..3)
            .map(|i| {
                let label = ["selected, read", "read", "unread"][i];
                let y = list.y + i as u16;
                format!("-- {label}\n{}", runs(&buf, y, list.x, list.right()))
            })
            .collect();
        insta::assert_snapshot!("list_row_styles", rows.join("\n"));
    }

    #[test]
    fn the_selected_row_carries_the_selection_band_across_its_whole_width() {
        let t = probed();
        for (w, h, selected) in [(160, 40, 0), (80, 24, 1), (120, 30, 2)] {
            let mut a = fetched(sample());
            a.theme = t;
            a.selected = selected;
            let buf = render(&mut a, w, h);
            let list = a.hits.list;
            for y in list.y..list.bottom() {
                for x in list.x..list.right() {
                    let banded = buf[(x, y)].bg == t.sel_bg;
                    let on_selection = y == list.y + selected as u16;
                    assert_eq!(banded, on_selection, "{w}x{h}: cell {x},{y}");
                }
            }
        }
    }

    #[test]
    fn the_band_runs_unbroken_past_wide_glyphs() {
        let t = probed();
        let mut a = wide_titles();
        a.theme = t;
        for selected in 0..a.stories.len() {
            a.selected = selected;
            let buf = render(&mut a, 160, 40);
            let list = a.hits.list;
            let y = list.y + selected as u16;
            let mut x = list.x;
            while x < list.right() {
                let cell = &buf[(x, y)];
                assert_eq!(cell.bg, t.sel_bg, "row {selected}, cell {x}");
                x += (width(cell.symbol()) as u16).max(1);
            }
        }
    }

    #[test]
    fn links_are_underlined_in_the_link_colour() {
        for (mut a, w, h) in [(fetched(sample()), 160, 40), (rich(), 44, 64)] {
            a.zoom = w < 100;
            let buf = render(&mut a, w, h);
            let title = a.selected_story().unwrap().title.clone();
            let mut links = 0;
            for (r, target) in &a.hits.zones {
                let Target::Open(_) = target else { continue };
                let text: String = (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect();
                if title.starts_with(text.trim_end()) {
                    continue; // the title opens the article too, drawn as a title
                }
                for x in r.x..r.right() {
                    let c = &buf[(x, r.y)];
                    assert_eq!(c.fg, a.theme.link, "`{text}` at {x},{}", r.y);
                    assert!(c.modifier.contains(Modifier::UNDERLINED), "`{text}`");
                }
                links += 1;
            }
            // Article, thread, and the comment's link: whole or wrapped.
            assert!(links >= 3, "{w}x{h}: {links} links");
        }
    }

    #[test]
    fn the_posters_name_is_in_the_accent_colour() {
        let mut a = fetched(sample());
        let buf = render(&mut a, 160, 40);
        let t = a.theme;
        let y = row_with(&buf, "▾ pg");
        let x = find(&buf, y, "pg  ").unwrap();
        for c in cells(&buf, y, x, 2) {
            assert_eq!(c.fg, t.accent);
            assert!(c.modifier.contains(Modifier::BOLD));
        }
        let y = row_with(&buf, "▾ alice");
        let x = find(&buf, y, "alice").unwrap();
        for c in cells(&buf, y, x, 5) {
            assert_eq!(c.fg, t.fg, "not the poster");
            assert!(c.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn quoted_paragraphs_are_italic_and_muted() {
        let mut a = fetched(sample());
        let buf = render(&mut a, 160, 40);
        let t = a.theme;
        let y = row_with(&buf, "> quoted");
        let x = find(&buf, y, "> quoted").unwrap();
        for c in cells(&buf, y, x, 8) {
            assert_eq!(c.fg, t.muted);
            assert!(c.modifier.contains(Modifier::ITALIC));
        }
        let y = row_with(&buf, "reply from the author");
        let x = find(&buf, y, "reply").unwrap();
        for c in cells(&buf, y, x, 5) {
            assert_eq!(c.fg, t.fg, "the reply itself is not quoted");
            assert!(!c.modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn read_titles_are_muted_unless_selected() {
        let mut a = fetched(sample());
        a.read.extend([1, 2]);
        let buf = render(&mut a, 160, 40);
        let t = a.theme;
        let title = |i: usize| {
            let y = a.hits.list.y + i as u16;
            let x = find(&buf, y, &a.stories[i].title[..6]).unwrap();
            buf[(x, y)].clone()
        };
        let (selected, read, unread) = (title(0), title(1), title(2));
        assert_eq!(selected.fg, t.fg, "the cursor's row reads in full");
        assert!(selected.modifier.contains(Modifier::BOLD));
        assert_eq!(read.fg, t.muted);
        assert!(!read.modifier.contains(Modifier::BOLD));
        assert_eq!(unread.fg, t.fg);
    }

    #[test]
    fn big_scores_stand_out_and_small_ones_recede() {
        let mut a = front_page(3); // 439, 87, 1204
        a.selected = 2;
        let buf = render(&mut a, 160, 40);
        let t = a.theme;
        let list = a.hits.list;
        let score = |i: u16, s: &str| buf[(find(&buf, list.y + i, s).unwrap(), list.y + i)].clone();
        let big = score(0, "439");
        assert_eq!(big.fg, t.accent);
        assert!(big.modifier.contains(Modifier::BOLD));
        let small = score(1, "87");
        assert_eq!(small.fg, t.muted);
        assert!(!small.modifier.contains(Modifier::BOLD));
        assert_eq!(score(2, "1204").fg, t.accent, "big on the selected row too");
    }

    #[test]
    fn an_old_fetch_turns_the_status_dot_from_green_to_amber() {
        for (age, colour) in [(90, Color::Green), (601, Color::Yellow)] {
            let mut a = fetched(sample());
            a.last_fetch = now_secs() - age;
            let buf = render(&mut a, 120, 30);
            let x = find(&buf, 0, "●").expect("a status dot");
            assert_eq!(buf[(x, 0)].fg, colour, "{age}s old");
        }
    }

    type Setup = fn(&mut App);

    #[test]
    fn no_color_draws_the_same_screens_without_any_colour() {
        let setups: [(u16, u16, Setup); 11] = [
            (160, 40, |_| {}),
            (80, 24, |a| {
                a.read.insert(2);
                a.selected = 2;
            }),
            (100, 30, |a| a.show_help = true),
            (80, 24, |a| a.error = Some("timed out".into())),
            (80, 24, |a| a.prompt = Some("rust".into())),
            (80, 24, |a| a.note("opened https://example.com/post")),
            (240, 40, |a| a.zoom = true),
            (120, 30, |a| {
                a.selected = 1;
                a.pending.insert(2);
            }),
            (120, 30, |a| {
                a.selected = 1;
                a.thread_errors.insert(2, "HTTP 503".into());
            }),
            (120, 30, |a| a.last_fetch = now_secs() - 900),
            (60, 24, |a| {
                a.stories.clear();
                a.loading_feed = true;
            }),
        ];
        let themes = [
            Theme::resolve_with(Probed::default(), true),
            Theme::resolve_with(
                Probed {
                    fg: Some((220, 220, 210)),
                    bg: Some((24, 24, 28)),
                },
                true,
            ),
        ];
        let app = || {
            let mut a = fetched(sample());
            a.stories[0].score = 999; // an accented score
            a
        };
        for (i, (w, h, setup)) in setups.into_iter().enumerate() {
            let mut colour = app();
            setup(&mut colour);
            let expected = screen(&render(&mut colour, w, h));
            for theme in themes {
                let mut plain = app();
                plain.theme = theme;
                setup(&mut plain);
                let buf = render(&mut plain, w, h);
                for (x, y) in (0..h).flat_map(|y| (0..w).map(move |x| (x, y))) {
                    let c = &buf[(x, y)];
                    assert_eq!(
                        (c.fg, c.bg),
                        (Color::Reset, Color::Reset),
                        "setup {i}: `{}` at {x},{y}",
                        c.symbol()
                    );
                }
                assert_eq!(screen(&buf), expected, "setup {i}: same layout");
            }
        }
    }

    #[test]
    fn the_rail_underline_sits_exactly_under_the_active_tab_label() {
        let tabs = [
            (Feed::Top, "Top"),
            (Feed::New, "New"),
            (Feed::Best, "Best"),
            (Feed::Ask, "Ask"),
            (Feed::Show, "Show"),
            (Feed::Jobs, "Jobs"),
            (Feed::Search, "⌕ 日本語 rust"),
        ];
        for w in [160u16, 60, 44, 30] {
            for (feed, label) in tabs {
                let mut a = fetched(sample());
                a.query = "日本語 rust".into();
                a.feed = feed;
                let buf = render(&mut a, w, 12);
                let heavy: Vec<u16> = (0..w).filter(|&x| buf[(x, 2)].symbol() == "━").collect();

                // The label's glyphs as drawn, cut short only by the edge.
                let glyphs: Vec<(u16, &str)> = {
                    let mut out = Vec::new();
                    let mut x = 0;
                    while x < w {
                        let s = buf[(x, 1)].symbol();
                        out.push((x, s));
                        x += (width(s) as u16).max(1);
                    }
                    out
                };
                let want: Vec<String> = label.chars().map(String::from).collect();
                let shown = (0..glyphs.len()).find_map(|i| {
                    let n = glyphs[i..]
                        .iter()
                        .zip(&want)
                        .take_while(|((_, g), l)| g == l)
                        .count();
                    (n == want.len() || (n > 0 && i + n == glyphs.len())).then(|| &glyphs[i..i + n])
                });
                let expected: Vec<u16> = shown
                    .map(|g| {
                        let (start, _) = g[0];
                        let (last, s) = g[g.len() - 1];
                        (start..last + width(s) as u16).collect()
                    })
                    .unwrap_or_default();
                assert_eq!(
                    heavy,
                    expected,
                    "{label} at {w} wide:\n{}\n{}",
                    row(&buf, 1),
                    row(&buf, 2)
                );
                for &x in &heavy {
                    assert_eq!(buf[(x, 2)].fg, a.theme.accent);
                }
                for (x, _) in shown.unwrap_or_default() {
                    assert_eq!(buf[(*x, 1)].fg, a.theme.accent, "the label is accented");
                }
            }
        }
    }

    #[test]
    fn help_dims_everything_behind_it_and_nothing_inside() {
        let mut a = fetched(sample());
        a.show_help = true;
        let buf = render(&mut a, 80, 24);
        let top = row_with(&buf, " keys ");
        let left = find(&buf, top, "┌").unwrap();
        let right = find(&buf, top, "┐").unwrap();
        let bottom = (top..24).find(|&y| buf[(left, y)].symbol() == "└").unwrap();
        let modal = Rect::new(left, top, right - left + 1, bottom - top + 1);
        for pos in buf.area.positions() {
            let dim = buf[pos].modifier.contains(Modifier::DIM);
            assert_eq!(dim, !modal.contains(pos), "cell {pos:?}");
        }
    }
}
