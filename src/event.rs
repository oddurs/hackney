//! Input handling: keys and mouse, resolved against the regions the last frame
//! recorded in `app.hits`.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};

use crate::app::{App, Target};
use crate::probe::Cut;

/// Two clicks on the same row inside this window count as a double-click,
/// which crossterm does not report on its own.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Lines or rows per notch of the wheel.
const SCROLL: isize = 3;

/// How long to go on discarding a colour reply that is missing its end.
const OSC_WINDOW: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Input {
    /// The row last clicked, and when.
    last_click: Option<(usize, Instant)>,
    /// Set while discarding the rest of a probe reply that came in after the
    /// probe stopped waiting. crossterm reads a late reply's `ESC ]` as
    /// Alt-] and the rest as typed keys — which would switch feeds and
    /// refresh; the tail of one the probe stopped partway through has no
    /// marker at all.
    swallow: Option<(Cut, Instant)>,
}

/// A click is resolved against the regions of the last frame, so it has to
/// wait for a new one if anything before it changed the screen.
pub fn is_click(ev: &Event) -> bool {
    matches!(ev, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(_)))
}

impl Input {
    /// Input that first discards the rest of any reply the colour probe
    /// stopped reading partway through.
    pub fn after_probe(cut: Option<Cut>) -> Self {
        Input {
            swallow: cut.map(|c| (c, Instant::now())),
            ..Input::default()
        }
    }

    /// Returns whether the screen may have changed.
    pub fn handle(&mut self, app: &mut App, ev: Event) -> bool {
        match ev {
            Event::Key(k) => {
                // Ctrl-C always quits, even mid-search.
                if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    app.quit = true;
                } else {
                    self.key(app, k);
                }
                true
            }
            Event::Mouse(m) => self.mouse(app, m),
            Event::Resize(..) => true,
            _ => false,
        }
    }

    pub fn key(&mut self, app: &mut App, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }

        // An OSC reply ends with ST (read as Alt-\) or BEL (read as Ctrl-G);
        // a CSI one with a final byte, read as that character.
        if let Some((cut, since)) = self.swallow {
            let end = match cut {
                Cut::Osc => {
                    (k.code == KeyCode::Char('\\') && k.modifiers.contains(KeyModifiers::ALT))
                        || (k.code == KeyCode::Char('g')
                            && k.modifiers.contains(KeyModifiers::CONTROL))
                }
                Cut::Csi => matches!(k.code, KeyCode::Char('@'..='~')),
            };
            if end {
                self.swallow = None;
                return;
            }
            if since.elapsed() < OSC_WINDOW {
                return;
            }
            self.swallow = None;
        }
        if k.code == KeyCode::Char(']') && k.modifiers.contains(KeyModifiers::ALT) {
            self.swallow = Some((Cut::Osc, Instant::now()));
            return;
        }

        if let Some(q) = app.prompt.as_mut() {
            match k.code {
                KeyCode::Esc => app.prompt = None,
                KeyCode::Enter => {
                    let q = app.prompt.take().unwrap_or_default();
                    if !q.trim().is_empty() {
                        app.search(q.trim().to_string());
                    }
                }
                KeyCode::Backspace => {
                    q.pop();
                }
                KeyCode::Char(c)
                    if !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    q.push(c)
                }
                _ => {}
            }
            return;
        }

        if app.show_help && !matches!(k.code, KeyCode::Char('?' | 'q') | KeyCode::Esc) {
            return;
        }

        let page = app.hits.detail.height.saturating_sub(2).max(1) as isize;
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
            KeyCode::Char('q') if app.show_help => app.show_help = false,
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('?') => app.show_help = !app.show_help,
            KeyCode::Esc => {
                if app.show_help {
                    app.show_help = false;
                } else if app.zoom {
                    app.zoom = false;
                } else {
                    app.error = None;
                }
            }

            KeyCode::Char('j') | KeyCode::Down => app.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_sel(-1),
            KeyCode::Char('g') | KeyCode::Home => app.select(0),
            KeyCode::Char('G') | KeyCode::End => app.select(app.stories.len().saturating_sub(1)),
            KeyCode::PageDown => app.move_sel(10),
            KeyCode::PageUp => app.move_sel(-10),

            KeyCode::Char('J') => app.scroll_detail(1),
            KeyCode::Char('K') => app.scroll_detail(-1),
            KeyCode::Char(' ') => app.scroll_detail(page),
            KeyCode::Char('b') => app.scroll_detail(-page),
            KeyCode::Char('n') => app.jump(true),
            KeyCode::Char('p') => app.jump(false),
            KeyCode::Char('x') => app.fold_current(),
            KeyCode::Char('z') => app.zoom = !app.zoom,

            KeyCode::Tab => app.cycle_feed(1),
            KeyCode::BackTab => app.cycle_feed(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c.to_digit(10).unwrap_or(1) as usize - 1;
                if let Some(feed) = app.tabs().get(i).copied() {
                    app.set_feed(feed);
                }
            }

            KeyCode::Enter | KeyCode::Char('o') => app.open_article(),
            KeyCode::Char('c') => app.open_discussion(),
            KeyCode::Char('r') => app.refresh(),
            KeyCode::Char('/') => app.prompt = Some(String::new()),
            _ => {}
        }
    }

    /// Returns whether the event did anything: with mouse capture on, every
    /// movement is reported, and those shouldn't cost a frame each.
    pub fn mouse(&mut self, app: &mut App, m: MouseEvent) -> bool {
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if !app.show_help => {
                let d = if m.kind == MouseEventKind::ScrollDown {
                    SCROLL
                } else {
                    -SCROLL
                };
                // Anywhere but the list scrolls the comments.
                if in_rect(app.hits.list, x, y) {
                    app.scroll_list(d);
                } else {
                    app.scroll_detail(d);
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) if app.show_help => {
                app.show_help = false;
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let target = app
                    .hits
                    .zones
                    .iter()
                    .find(|(r, _)| in_rect(*r, x, y))
                    .map(|(_, t)| t.clone());
                match target {
                    Some(Target::Tab(feed)) if feed != app.feed => app.set_feed(feed),
                    Some(Target::Row(i)) => {
                        let now = Instant::now();
                        let double = self.last_click.is_some_and(|(li, lt)| {
                            li == i && now.duration_since(lt) < DOUBLE_CLICK
                        });
                        // A third click starts over rather than opening it twice.
                        self.last_click = if double { None } else { Some((i, now)) };
                        app.select(i);
                        if double {
                            app.open_article();
                        }
                    }
                    Some(Target::Fold(i)) => app.toggle_fold(i),
                    Some(Target::Open(url)) => app.open_url(&url),
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }
}

fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    r.width > 0 && r.height > 0 && x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::comment;
    use crate::app::{Listing, Msg, Thread};
    use crate::hn::{Feed, Story};
    use crate::theme::Theme;
    use crate::ui;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;

    fn story(id: u64) -> Story {
        Story {
            id,
            title: format!("story {id}"),
            by: "pg".into(),
            url: Some(format!("https://example.com/{id}")),
            ..Story::default()
        }
    }

    /// The first `requested` of stories 1..=n on the Top feed, as if just fetched.
    fn page(n: u64, requested: usize) -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.loading_feed = true;
        a.on_msg(Msg::Feed {
            generation: a.generation,
            result: Ok(Listing {
                ids: (1..=n).collect(),
                requested,
                stories: (1..=requested as u64).map(story).collect(),
            }),
        });
        a
    }

    fn app(n: u64) -> App {
        page(n, n as usize)
    }

    /// Twelve top-level comments on story `id`, each with a link and a reply:
    /// enough to scroll.
    fn with_thread(a: &mut App, id: u64) {
        let comments = (0..12)
            .map(|i| {
                let text = format!("comment {i}, <a href=\"https://link.example/{i}\">link</a>");
                let reply = comment("replier", "a reply", 1, vec![]);
                comment(&format!("user{i}"), &text, 0, vec![reply])
            })
            .collect();
        a.threads.insert(id, Thread::new(story(id), comments));
    }

    fn draw(a: &mut App, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ui::draw(f, a)).unwrap();
        term.backend().buffer().clone()
    }

    fn text_at(buf: &Buffer, r: Rect) -> String {
        (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect()
    }

    /// Where the last frame drew `target`.
    fn zone(a: &App, target: &Target) -> Rect {
        a.hits
            .zones
            .iter()
            .find(|(_, t)| t == target)
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("{target:?} was not drawn"))
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, NONE))
    }

    fn ch(c: char) -> Event {
        key(KeyCode::Char(c))
    }

    fn chord(c: char, m: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), m))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: NONE,
        })
    }

    fn click(r: Rect) -> Event {
        mouse(MouseEventKind::Down(MouseButton::Left), r.x, r.y)
    }

    fn typed(input: &mut Input, a: &mut App, s: &str) {
        for c in s.chars() {
            input.handle(a, ch(c));
        }
    }

    fn notice(a: &mut App) -> Option<String> {
        a.notice.take().map(|n| n.0)
    }

    /// What a stray key could change.
    #[allow(clippy::type_complexity)]
    fn state(
        a: &App,
    ) -> (
        Feed,
        usize,
        usize,
        usize,
        u64,
        bool,
        bool,
        Option<String>,
        bool,
        Option<String>,
    ) {
        (
            a.feed,
            a.selected,
            a.offset,
            a.detail_scroll,
            a.generation,
            a.zoom,
            a.show_help,
            a.prompt.clone(),
            a.quit,
            a.notice.as_ref().map(|n| n.0.clone()),
        )
    }

    // ------------------------------------------------------------------ keys

    #[test]
    fn j_k_and_the_arrow_keys_move_one_story_and_stop_at_the_ends() {
        let mut a = app(5);
        let mut i = Input::default();
        let steps = [
            (ch('j'), 1),
            (key(KeyCode::Down), 2),
            (ch('k'), 1),
            (key(KeyCode::Up), 0),
            (ch('k'), 0),
        ];
        for (ev, want) in steps {
            assert!(i.handle(&mut a, ev.clone()));
            assert_eq!(a.selected, want, "after {ev:?}");
        }
        for _ in 0..10 {
            i.handle(&mut a, ch('j'));
        }
        assert_eq!(a.selected, 4);
    }

    #[test]
    fn g_and_home_go_to_the_first_story_and_shift_g_and_end_to_the_last() {
        let mut a = app(40);
        let mut i = Input::default();
        for (ev, want) in [
            (ch('G'), 39),
            (ch('g'), 0),
            (key(KeyCode::End), 39),
            (key(KeyCode::Home), 0),
        ] {
            i.handle(&mut a, ev.clone());
            assert_eq!(a.selected, want, "after {ev:?}");
        }
    }

    #[test]
    fn page_down_and_page_up_move_ten_stories() {
        let mut a = app(25);
        let mut i = Input::default();
        for (code, want) in [
            (KeyCode::PageDown, 10),
            (KeyCode::PageDown, 20),
            (KeyCode::PageDown, 24),
            (KeyCode::PageUp, 14),
            (KeyCode::PageUp, 4),
            (KeyCode::PageUp, 0),
        ] {
            i.handle(&mut a, key(code));
            assert_eq!(a.selected, want, "after {code:?}");
        }
    }

    #[test]
    fn shift_j_and_k_scroll_the_comments_a_line_and_space_and_b_a_page() {
        let mut a = app(3);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let page = a.hits.detail.height as usize - 2;
        let mut i = Input::default();
        for (ev, want) in [
            (ch('J'), 1),
            (ch('J'), 2),
            (ch('K'), 1),
            (ch(' '), 1 + page),
            (ch('b'), 1),
            (ch('b'), 0),
            (ch('K'), 0),
        ] {
            i.handle(&mut a, ev.clone());
            assert_eq!(a.detail_scroll, want, "after {ev:?}");
        }
        assert_eq!(a.selected, 0, "the list stays put");
    }

    #[test]
    fn n_and_p_step_between_top_level_comments() {
        let mut a = app(3);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let roots: Vec<usize> = a.layout.roots.iter().map(|r| r.0).collect();
        assert_eq!(roots.len(), 12);
        let mut i = Input::default();
        for (c, want) in [('n', roots[0]), ('n', roots[1]), ('p', roots[0]), ('p', 0)] {
            i.handle(&mut a, ch(c));
            assert_eq!(a.detail_scroll, want, "after {c}");
        }
        a.detail_scroll = roots[11];
        i.handle(&mut a, ch('n'));
        assert_eq!(a.detail_scroll, roots[11], "no further than the last");
    }

    #[test]
    fn x_folds_the_top_level_thread_being_read_and_keeps_it_in_place() {
        let mut a = app(3);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let mut i = Input::default();
        i.handle(&mut a, ch('n'));
        i.handle(&mut a, ch('n'));
        let (line, node) = a.layout.roots[1];
        i.handle(&mut a, ch('x'));
        let folded = |a: &App| -> Vec<usize> {
            let t = a.thread().unwrap();
            (0..t.nodes.len()).filter(|&n| t.nodes[n].folded).collect()
        };
        assert_eq!(folded(&a), [node]);
        assert_eq!(a.detail_scroll, line);

        // Partway into it, `x` finds its head and opens it again.
        draw(&mut a, 160, 40);
        i.handle(&mut a, ch('J'));
        i.handle(&mut a, ch('x'));
        assert!(folded(&a).is_empty());
        assert_eq!(a.detail_scroll, line);

        // Above the first comment, it is the first that folds.
        a.detail_scroll = 0;
        draw(&mut a, 160, 40);
        i.handle(&mut a, ch('x'));
        assert_eq!(folded(&a), [a.layout.roots[0].1]);
    }

    #[test]
    fn z_zooms_and_esc_unzooms_before_it_clears_an_error() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('z'));
        assert!(a.zoom);
        i.handle(&mut a, ch('z'));
        assert!(!a.zoom);
        i.handle(&mut a, ch('z'));
        a.error = Some("boom".into());
        i.handle(&mut a, key(KeyCode::Esc));
        assert!(!a.zoom && a.error.is_some());
        i.handle(&mut a, key(KeyCode::Esc));
        assert!(a.error.is_none());
    }

    #[test]
    fn tab_and_shift_tab_cycle_through_the_feeds_and_wrap() {
        let mut a = app(3);
        let mut i = Input::default();
        for (code, want) in [
            (KeyCode::Tab, Feed::New),
            (KeyCode::BackTab, Feed::Top),
            (KeyCode::BackTab, Feed::Jobs),
            (KeyCode::Tab, Feed::Top),
        ] {
            i.handle(&mut a, key(code));
            assert_eq!(a.feed, want, "after {code:?}");
        }
        a.query = "rust".into();
        i.handle(&mut a, key(KeyCode::BackTab));
        assert_eq!(a.feed, Feed::Search);
        i.handle(&mut a, key(KeyCode::Tab));
        assert_eq!(a.feed, Feed::Top);
    }

    #[test]
    fn number_keys_open_the_feed_at_that_position() {
        let mut a = app(3);
        let mut i = Input::default();
        for (c, feed) in ('1'..='6').zip(Feed::TABS) {
            i.handle(&mut a, ch(c));
            assert_eq!(a.feed, feed, "after {c}");
        }
        // No seventh tab until there has been a search.
        let before = state(&a);
        for c in ['7', '8', '9', '0'] {
            i.handle(&mut a, ch(c));
            assert_eq!(state(&a), before, "after {c}");
        }
        a.query = "rust".into();
        i.handle(&mut a, ch('7'));
        assert_eq!(a.feed, Feed::Search);
    }

    #[test]
    fn enter_and_o_open_the_article_and_c_the_discussion() {
        let mut a = app(3);
        let mut i = Input::default();
        let article = Some("opened https://example.com/1".to_string());
        i.handle(&mut a, key(KeyCode::Enter));
        assert_eq!(notice(&mut a), article);
        i.handle(&mut a, ch('o'));
        assert_eq!(notice(&mut a), article);
        i.handle(&mut a, ch('c'));
        let discussion = "opened https://news.ycombinator.com/item?id=1";
        assert_eq!(notice(&mut a).as_deref(), Some(discussion));
        // A text post has no article: its discussion is what opens.
        a.stories[0].url = None;
        i.handle(&mut a, key(KeyCode::Enter));
        assert_eq!(notice(&mut a).as_deref(), Some(discussion));
    }

    #[test]
    fn r_fetches_everything_again_behind_the_list_on_screen() {
        let mut a = app(3);
        let generation = a.generation;
        Input::default().handle(&mut a, ch('r'));
        assert!(a.loading_feed && a.generation > generation);
        assert_eq!(a.stories.len(), 3);
        assert!(a.pending.contains(&1), "the story being read");
    }

    #[test]
    fn q_quits() {
        let mut a = app(3);
        Input::default().handle(&mut a, ch('q'));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        let setups: [fn(&mut App, &mut Input); 4] = [
            |_, _| {},
            |a, _| a.prompt = Some("half a qu".into()),
            |a, _| a.show_help = true,
            |a, i| {
                i.handle(a, chord(']', ALT));
            },
        ];
        for (n, setup) in setups.into_iter().enumerate() {
            let mut a = app(3);
            let mut i = Input::default();
            setup(&mut a, &mut i);
            i.handle(&mut a, chord('c', CTRL));
            assert!(a.quit, "setup {n}");
        }
    }

    #[test]
    fn question_mark_opens_and_closes_the_help() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('?'));
        assert!(a.show_help);
        i.handle(&mut a, ch('?'));
        assert!(!a.show_help);
    }

    #[test]
    fn the_help_swallows_every_key_but_those_that_close_it() {
        let mut a = app(20);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let mut i = Input::default();
        i.handle(&mut a, ch('?'));
        let before = state(&a);
        let keys = [
            ch('j'),
            ch('k'),
            ch('G'),
            key(KeyCode::Down),
            key(KeyCode::PageDown),
            ch('J'),
            ch(' '),
            ch('n'),
            ch('x'),
            ch('z'),
            key(KeyCode::Tab),
            key(KeyCode::BackTab),
            ch('2'),
            key(KeyCode::Enter),
            ch('o'),
            ch('c'),
            ch('r'),
            ch('/'),
            ch('w'),
        ];
        for ev in keys {
            i.handle(&mut a, ev.clone());
            assert_eq!(state(&a), before, "{ev:?} got through");
        }
        assert!(!a.loading_feed, "no refresh");

        i.handle(&mut a, ch('q'));
        assert!(
            !a.show_help && !a.quit,
            "q closes the help rather than quitting"
        );
        a.zoom = true;
        i.handle(&mut a, ch('?'));
        i.handle(&mut a, key(KeyCode::Esc));
        assert!(!a.show_help && a.zoom, "esc closes only the help");
    }

    #[test]
    fn a_click_anywhere_closes_the_help_and_does_nothing_else() {
        let mut a = app(20);
        draw(&mut a, 160, 40);
        let row = zone(&a, &Target::Row(5));
        let mut i = Input::default();
        i.handle(&mut a, ch('?'));
        draw(&mut a, 160, 40);
        assert!(i.handle(&mut a, click(row)));
        assert!(!a.show_help);
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn the_wheel_does_nothing_while_the_help_is_open() {
        let mut a = app(60);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let (list, detail) = (a.hits.list, a.hits.detail);
        let mut i = Input::default();
        i.handle(&mut a, ch('?'));
        for r in [list, detail] {
            assert!(!i.handle(&mut a, mouse(MouseEventKind::ScrollDown, r.x + 1, r.y + 1)));
        }
        assert_eq!((a.offset, a.detail_scroll), (0, 0));
        assert!(a.show_help);
    }

    // ---------------------------------------------------------------- prompt

    #[test]
    fn slash_opens_a_prompt_that_takes_what_is_typed() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('/'));
        assert_eq!(a.prompt.as_deref(), Some(""));
        typed(&mut i, &mut a, "rust lang");
        assert_eq!(a.prompt.as_deref(), Some("rust lang"));
        for _ in 0..5 {
            i.handle(&mut a, key(KeyCode::Backspace));
        }
        assert_eq!(a.prompt.as_deref(), Some("rust"));
        i.handle(&mut a, chord('R', KeyModifiers::SHIFT));
        assert_eq!(a.prompt.as_deref(), Some("rustR"));
        for _ in 0..10 {
            i.handle(&mut a, key(KeyCode::Backspace));
        }
        assert_eq!(a.prompt.as_deref(), Some(""), "backspace stops at empty");
    }

    #[test]
    fn keys_typed_into_the_prompt_are_text_not_commands() {
        let mut a = app(20);
        let mut i = Input::default();
        i.handle(&mut a, ch('/'));
        let text = "qjk12?rzxgGocn/ ";
        let mut before = state(&a);
        typed(&mut i, &mut a, text);
        before.7 = Some(text.into());
        assert_eq!(state(&a), before);
        assert!(!a.loading_feed);
    }

    #[test]
    fn esc_closes_the_prompt_without_searching() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('/'));
        typed(&mut i, &mut a, "rust");
        let generation = a.generation;
        i.handle(&mut a, key(KeyCode::Esc));
        assert!(a.prompt.is_none() && a.query.is_empty());
        assert_eq!((a.feed, a.generation), (Feed::Top, generation));
    }

    #[test]
    fn enter_on_a_blank_prompt_closes_it_without_searching() {
        for blank in ["", "   "] {
            let mut a = app(3);
            let mut i = Input::default();
            i.handle(&mut a, ch('/'));
            typed(&mut i, &mut a, blank);
            let generation = a.generation;
            i.handle(&mut a, key(KeyCode::Enter));
            assert!(a.prompt.is_none() && a.query.is_empty(), "{blank:?}");
            assert_eq!((a.feed, a.generation), (Feed::Top, generation));
            assert!(!a.tabs().contains(&Feed::Search));
        }
    }

    #[test]
    fn enter_searches_for_what_was_typed_without_its_surrounding_spaces() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('/'));
        typed(&mut i, &mut a, "  rust lang  ");
        i.handle(&mut a, key(KeyCode::Enter));
        assert!(a.prompt.is_none());
        assert_eq!(a.query, "rust lang");
        assert_eq!(a.feed, Feed::Search);
        assert!(a.loading_feed && a.tabs().contains(&Feed::Search));
    }

    #[test]
    fn ctrl_and_alt_chords_are_not_typed_into_the_prompt() {
        let mut a = app(3);
        let mut i = Input::default();
        i.handle(&mut a, ch('/'));
        for ev in [
            chord('a', CTRL),
            chord('b', ALT),
            chord('d', CTRL | ALT),
            chord('g', CTRL),
            key(KeyCode::Tab),
            key(KeyCode::Down),
        ] {
            i.handle(&mut a, ev.clone());
            assert_eq!(a.prompt.as_deref(), Some(""), "{ev:?}");
        }
        assert_eq!(a.feed, Feed::Top);
    }

    // --------------------------------------------------- late colour replies

    /// How crossterm reads a terminal's answer to the background-colour
    /// query that turns up after the probe stopped waiting: `ESC ]` as Alt-],
    /// the body as typed keys, then the terminator.
    fn colour_reply(end: Event) -> Vec<Event> {
        let mut evs = vec![chord(']', ALT)];
        // Digits switch feeds, `r` refreshes, `g` and `b` move, `/` opens the
        // prompt: all of it would act if let through.
        evs.extend("11;rgb:2e2e/1b1b/rrrr/g".chars().map(ch));
        evs.push(end);
        evs
    }

    #[test]
    fn a_colour_reply_arriving_late_is_swallowed_whole() {
        for end in [chord('\\', ALT), chord('g', CTRL)] {
            let mut a = app(20);
            a.select(5);
            let before = state(&a);
            let mut i = Input::default();
            for ev in colour_reply(end.clone()) {
                i.handle(&mut a, ev);
            }
            assert_eq!(state(&a), before, "ended by {end:?}");
            assert!(!a.loading_feed);
            i.handle(&mut a, ch('j'));
            assert_eq!(a.selected, 6, "keys are keys again after {end:?}");
        }
    }

    #[test]
    fn a_plain_backslash_or_g_does_not_end_a_colour_reply() {
        let mut a = app(20);
        a.select(5);
        let mut i = Input::default();
        i.handle(&mut a, chord(']', ALT));
        i.handle(&mut a, ch('\\'));
        i.handle(&mut a, ch('g'));
        i.handle(&mut a, chord('g', ALT));
        assert_eq!(a.selected, 5);
        i.handle(&mut a, chord('\\', ALT));
        i.handle(&mut a, ch('g'));
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn the_rest_of_a_reply_the_probe_cut_short_is_not_typed() {
        // The tail of an OSC reply: no `ESC ]` to mark it, just its end.
        let mut a = app(20);
        let mut i = Input::after_probe(Some(Cut::Osc));
        for c in "/1c1c".chars() {
            i.handle(&mut a, ch(c));
        }
        i.handle(&mut a, chord('\\', ALT));
        assert!(a.prompt.is_none(), "the `/` in it opened the prompt");
        i.handle(&mut a, ch('j'));
        assert_eq!(a.selected, 1, "real keys after it work");

        // The tail of DA1 ends at its final byte.
        let mut a = app(20);
        let mut i = Input::after_probe(Some(Cut::Csi));
        for c in "2;22c".chars() {
            i.handle(&mut a, ch(c));
        }
        i.handle(&mut a, ch('j'));
        assert_eq!((a.selected, a.feed), (1, Feed::Top));
    }

    #[test]
    fn keys_work_again_once_a_colour_reply_goes_quiet_without_its_end() {
        let mut a = app(20);
        let mut i = Input::default();
        i.handle(&mut a, chord(']', ALT));
        i.handle(&mut a, ch('j'));
        assert_eq!(a.selected, 0, "swallowed while the reply is due");
        i.swallow = i
            .swallow
            .map(|(c, t)| (c, t - OSC_WINDOW - Duration::from_millis(1)));
        i.handle(&mut a, ch('j'));
        assert_eq!(a.selected, 1);
        assert!(i.swallow.is_none());
        i.handle(&mut a, ch('j'));
        assert_eq!(a.selected, 2);
    }

    #[test]
    fn key_releases_are_ignored_and_repeats_act_as_presses() {
        let mut a = app(20);
        let mut i = Input::default();
        let as_kind = |code, kind| Event::Key(KeyEvent::new_with_kind(code, NONE, kind));
        let before = state(&a);
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('q'),
            KeyCode::Char('/'),
            KeyCode::Char('?'),
            KeyCode::Enter,
            KeyCode::Tab,
        ] {
            i.handle(&mut a, as_kind(code, KeyEventKind::Release));
            assert_eq!(state(&a), before, "release of {code:?}");
        }
        i.handle(&mut a, as_kind(KeyCode::Char('j'), KeyEventKind::Repeat));
        assert_eq!(a.selected, 1);
    }

    // ------------------------------------------------------------------ mouse

    #[test]
    fn the_wheel_over_the_list_scrolls_the_list_and_leaves_the_cursor() {
        let mut a = app(60);
        with_thread(&mut a, 1);
        draw(&mut a, 160, 40);
        let list = a.hits.list;
        let (x, y) = (list.x + 5, list.y + 5);
        let mut i = Input::default();
        assert!(i.handle(&mut a, mouse(MouseEventKind::ScrollDown, x, y)));
        assert_eq!((a.offset, a.selected, a.detail_scroll), (3, 0, 0));
        i.handle(&mut a, mouse(MouseEventKind::ScrollUp, x, y));
        i.handle(&mut a, mouse(MouseEventKind::ScrollUp, x, y));
        assert_eq!(a.offset, 0);
        for _ in 0..5 {
            i.handle(&mut a, mouse(MouseEventKind::ScrollDown, x, y));
        }
        draw(&mut a, 160, 40);
        assert_eq!(
            (a.offset, a.selected),
            (15, 0),
            "not pulled back to the cursor"
        );
    }

    #[test]
    fn the_wheel_anywhere_but_the_list_scrolls_the_comments() {
        for (w, h) in [(160, 40), (100, 40)] {
            let mut a = app(60);
            with_thread(&mut a, 1);
            draw(&mut a, w, h);
            let d = a.hits.detail;
            let mut i = Input::default();
            let spots = [(d.x + 2, d.y + 2), (0, 0), (w - 1, 1), (0, h - 1)];
            for (n, (x, y)) in spots.into_iter().enumerate() {
                i.handle(&mut a, mouse(MouseEventKind::ScrollDown, x, y));
                assert_eq!(a.detail_scroll, 3 * (n + 1), "{w}x{h} at {x},{y}");
            }
            i.handle(&mut a, mouse(MouseEventKind::ScrollUp, d.x + 2, d.y + 2));
            assert_eq!(a.detail_scroll, 9);
            assert_eq!(a.offset, 0, "{w}x{h}: the list stayed");
        }
    }

    #[test]
    fn clicking_a_tab_opens_the_feed_drawn_there() {
        let mut a = app(5);
        a.query = "rust".into();
        let mut i = Input::default();
        for feed in [
            Feed::New,
            Feed::Best,
            Feed::Ask,
            Feed::Show,
            Feed::Jobs,
            Feed::Search,
            Feed::Top,
        ] {
            let buf = draw(&mut a, 160, 40);
            let r = zone(&a, &Target::Tab(feed));
            let label = match feed {
                Feed::Search => "⌕ rust",
                f => f.title(),
            };
            assert!(text_at(&buf, r).contains(label), "{feed:?}");
            // Its stretch of the rail below counts too.
            i.handle(&mut a, click(Rect { y: r.y + 1, ..r }));
            assert_eq!(a.feed, feed);
        }
    }

    #[test]
    fn clicking_the_tab_already_open_does_nothing() {
        let mut a = app(5);
        draw(&mut a, 160, 40);
        let generation = a.generation;
        let top = zone(&a, &Target::Tab(Feed::Top));
        Input::default().handle(&mut a, click(top));
        assert_eq!(a.generation, generation);
        assert!(!a.loading_feed && a.stories.len() == 5);
    }

    #[test]
    fn clicking_a_row_selects_the_story_drawn_on_it() {
        let mut a = app(60);
        let mut i = Input::default();
        draw(&mut a, 160, 40);
        let list = a.hits.list;
        for _ in 0..3 {
            i.handle(&mut a, mouse(MouseEventKind::ScrollDown, list.x, list.y));
        }
        let buf = draw(&mut a, 160, 40);
        let row = Rect::new(list.x, list.y + 2, list.width, 1);
        let drawn = text_at(&buf, row);
        i.handle(&mut a, click(row));
        let s = a.selected_story().unwrap();
        assert!(drawn.contains(&format!(" {}  ", a.selected + 1)), "{drawn}");
        assert!(
            drawn.contains(&s.title),
            "`{}` clicked on `{drawn}`",
            s.title
        );
        assert_eq!(a.selected, 11);
    }

    #[test]
    fn clicking_a_comment_header_folds_and_unfolds_it() {
        let mut a = app(3);
        with_thread(&mut a, 1);
        let buf = draw(&mut a, 160, 40);
        let node = a.layout.roots[2].1;
        let r = zone(&a, &Target::Fold(node));
        assert!(text_at(&buf, r).contains("▾ user2"));
        let mut i = Input::default();
        i.handle(&mut a, click(r));
        assert!(a.thread().unwrap().nodes[node].folded);
        let buf = draw(&mut a, 160, 40);
        assert!(text_at(&buf, r).contains("▸ user2"));
        i.handle(&mut a, click(r));
        assert!(!a.thread().unwrap().nodes[node].folded);
    }

    #[test]
    fn clicking_a_link_opens_that_link() {
        let mut a = app(3);
        with_thread(&mut a, 1);
        let buf = draw(&mut a, 160, 40);
        let url = "https://link.example/1";
        let r = zone(&a, &Target::Open(url.into()));
        assert_eq!(text_at(&buf, r), "link");
        let mut i = Input::default();
        i.handle(&mut a, click(r));
        assert_eq!(notice(&mut a), Some(format!("opened {url}")));
        // The title is the article's link.
        let title = zone(&a, &Target::Open("https://example.com/1".into()));
        assert!(text_at(&buf, title).contains("story 1"));
        i.handle(&mut a, click(title));
        assert_eq!(
            notice(&mut a).as_deref(),
            Some("opened https://example.com/1")
        );
    }

    #[test]
    fn clicking_where_nothing_was_drawn_does_nothing() {
        let mut a = app(3);
        draw(&mut a, 160, 40);
        let before = state(&a);
        let mut i = Input::default();
        // The nav line, the footer, empty list rows (the first just under the
        // last story), and off the screen.
        for (x, y) in [(0, 0), (80, 39), (5, 6), (5, 30), (159, 39), (500, 500)] {
            i.handle(&mut a, click(Rect::new(x, y, 1, 1)));
            assert_eq!(state(&a), before, "{x},{y}");
        }
    }

    #[test]
    fn only_the_left_button_clicks() {
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let r = zone(&a, &Target::Row(4));
        let mut i = Input::default();
        for b in [MouseButton::Right, MouseButton::Middle] {
            assert!(!i.handle(&mut a, mouse(MouseEventKind::Down(b), r.x, r.y)));
            assert!(!i.handle(&mut a, mouse(MouseEventKind::Down(b), r.x, r.y)));
        }
        assert_eq!(a.selected, 0);
        assert!(a.notice.is_none());
    }

    #[test]
    fn double_clicking_a_row_opens_its_article() {
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let r = zone(&a, &Target::Row(2));
        let mut i = Input::default();
        i.handle(&mut a, click(r));
        assert_eq!(
            (a.selected, notice(&mut a)),
            (2, None),
            "one click reads it"
        );
        draw(&mut a, 160, 40);
        i.handle(&mut a, click(r));
        assert_eq!(
            notice(&mut a).as_deref(),
            Some("opened https://example.com/3")
        );
    }

    #[test]
    fn clicks_on_two_different_rows_are_not_a_double_click() {
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let mut i = Input::default();
        let (two, three) = (zone(&a, &Target::Row(2)), zone(&a, &Target::Row(3)));
        i.handle(&mut a, click(two));
        i.handle(&mut a, click(three));
        assert_eq!((a.selected, notice(&mut a)), (3, None));
    }

    #[test]
    fn a_triple_click_opens_the_article_only_once() {
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let r = zone(&a, &Target::Row(2));
        let mut i = Input::default();
        let mut opened = 0;
        for _ in 0..3 {
            i.handle(&mut a, click(r));
            opened += usize::from(notice(&mut a).is_some());
        }
        assert_eq!(opened, 1);
    }

    #[test]
    fn two_clicks_further_apart_than_a_double_click_do_not_open() {
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let r = zone(&a, &Target::Row(2));
        let mut i = Input::default();
        i.handle(&mut a, click(r));
        i.last_click = i
            .last_click
            .map(|(row, t)| (row, t - DOUBLE_CLICK - Duration::from_millis(1)));
        i.handle(&mut a, click(r));
        assert_eq!(notice(&mut a), None);
    }

    #[test]
    fn only_a_button_press_is_a_click() {
        use MouseEventKind::*;
        assert!(is_click(&mouse(Down(MouseButton::Left), 0, 0)));
        assert!(is_click(&mouse(Down(MouseButton::Right), 3, 4)));
        for kind in [
            Up(MouseButton::Left),
            Drag(MouseButton::Left),
            Moved,
            ScrollDown,
            ScrollUp,
        ] {
            assert!(!is_click(&mouse(kind, 0, 0)), "{kind:?}");
        }
        for ev in [
            ch('j'),
            key(KeyCode::Enter),
            Event::Resize(80, 24),
            Event::FocusGained,
        ] {
            assert!(!is_click(&ev), "{ev:?}");
        }
    }

    #[test]
    fn mouse_movement_and_other_events_do_not_ask_for_a_redraw() {
        use MouseEventKind::*;
        let mut a = app(10);
        draw(&mut a, 160, 40);
        let mut i = Input::default();
        let quiet = [
            mouse(Moved, 10, 10),
            mouse(Drag(MouseButton::Left), 10, 10),
            mouse(Up(MouseButton::Left), 10, 10),
            mouse(ScrollLeft, 10, 10),
            mouse(ScrollRight, 10, 10),
            Event::FocusGained,
            Event::FocusLost,
            Event::Paste("rust".into()),
        ];
        for ev in quiet {
            assert!(!i.handle(&mut a, ev.clone()), "{ev:?}");
        }
        for ev in [
            Event::Resize(100, 30),
            ch('j'),
            mouse(ScrollDown, 10, 10),
            mouse(Down(MouseButton::Left), 10, 10),
        ] {
            assert!(i.handle(&mut a, ev.clone()), "{ev:?}");
        }
    }

    #[test]
    fn in_a_three_row_terminal_a_click_on_the_story_title_opens_it_not_a_tab() {
        let mut a = app(3);
        let buf = draw(&mut a, 20, 3);
        let title = zone(&a, &Target::Open("https://example.com/1".into()));
        assert!(text_at(&buf, title).contains("story 1"));
        let new = zone(&a, &Target::Tab(Feed::New));
        let x = new.x.max(title.x);
        assert!(x < title.right(), "a column of the title under the New tab");
        Input::default().handle(&mut a, click(Rect::new(x, title.y, 1, 1)));
        assert_eq!(a.feed, Feed::Top, "no tab is drawn on the title's row");
        assert_eq!(
            notice(&mut a).as_deref(),
            Some("opened https://example.com/1")
        );
    }

    #[test]
    fn growing_the_terminal_past_the_end_of_the_list_asks_for_the_next_page() {
        let mut a = page(100, 30);
        let mut i = Input::default();
        draw(&mut a, 160, 20);
        assert!(!a.loading_more, "a short pane has rows to spare");
        i.handle(&mut a, Event::Resize(160, 60));
        draw(&mut a, 160, 60);
        assert!(
            a.hits.list.height as usize > a.stories.len(),
            "rows left empty"
        );
        assert!(a.loading_more, "the pane can show more than is loaded");
    }
}
