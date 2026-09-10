//! End to end: the real binary, in a real pseudo-terminal, against a fake
//! Hacker News (`tests/support/fake_hn.rs`) and a link opener that records
//! instead of opening.
//!
//! The screen is rebuilt with a real terminal emulator (vt100), so what is
//! asserted is what a user would see. Every wait is on something only the
//! state being waited for draws: text already on screen elsewhere would let a
//! wait pass before the key or click had landed.
#![cfg(unix)]

#[path = "support/fake_hn.rs"]
mod fake_hn;

use fake_hn::{FakeHn, node, now, story_json};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: u16 = 30;
const COLS: u16 = 160;
const FOOTER: u16 = ROWS - 1;
/// The deadline for anything a test waits on.
const LOAD: Duration = Duration::from_secs(10);

// ------------------------------------------------------------------ the world

/// A Hacker News that answers from routes a test sets up, and a link opener
/// that writes each URL it is handed to a file.
struct World {
    hn: FakeHn,
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_exe(
            &dir.path().join("open-log"),
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$(dirname \"$0\")/opened\"\n",
        );
        World {
            hn: FakeHn::start(),
            dir,
        }
    }

    /// The front page: forty stories, the first with a thread worth reading,
    /// the second with one reply, the third a text post. Two new stories and
    /// one job; the other feeds are not there (404).
    fn standard() -> Self {
        let w = World::new();
        let hn = &w.hn;
        let ids: Vec<u64> = (1..=40).collect();
        hn.feed("topstories", &ids);
        for &id in &ids {
            hn.story(id, &headline(id), &[]);
        }
        hn.story(1, &headline(1), &[101, 104, 105, 106]);
        hn.tree(1, story_one_tree());
        hn.story(2, &headline(2), &[201]);
        hn.tree(
            2,
            json!([node(
                201,
                "octocat",
                "Story two has a single reply",
                json!([])
            )]),
        );
        hn.item(json!({
            "id": 3, "type": "story", "by": "octocat",
            "title": "Ask HN: How do you test a terminal app?",
            "text": "I drive the <i>real binary</i> in a pty.",
            "score": 42, "time": now() - 7200, "descendants": 0, "kids": [],
        }));
        hn.feed("newstories", &[501, 502]);
        hn.story(501, "Fresh off the press", &[]);
        hn.story(502, "Another new arrival", &[]);
        hn.feed("jobstories", &[601]);
        hn.story(601, "Example Corp is hiring", &[]);
        w
    }

    fn opener(&self) -> String {
        self.dir.path().join("open-log").display().to_string()
    }

    fn opened(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("opened"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Wait until the opener has been handed `n` links, and return them.
    fn wait_opened(&self, n: usize) -> Vec<String> {
        eventually(&format!("{n} opened link(s)"), || self.opened().len() >= n);
        self.opened()
    }
}

fn headline(id: u64) -> String {
    format!("Headline number {id} here")
}

/// alice (with bob, and carol under him), dave with a web link, erin with a
/// link that is not one, and frank's long reply to give `n` room to jump.
fn story_one_tree() -> Value {
    let long = |who: &str, n: usize| {
        (1..=n)
            .map(|i| format!("{who} paragraph {i} of a long reply."))
            .collect::<Vec<_>>()
            .join("<p>")
    };
    json!([
        node(
            101,
            "alice",
            &format!("Alice opens the thread.<p>{}", long("Alice", 4)),
            json!([node(
                102,
                "bob",
                "Bob answers alice directly.",
                json!([node(
                    103,
                    "carol",
                    "Carol goes one level deeper.",
                    json!([])
                )]),
            )]),
        ),
        node(
            104,
            "dave",
            "Dave points at <a href=\"https://example.com/docs\" rel=\"nofollow\">https://example.com/docs</a> for details.",
            json!([]),
        ),
        node(
            105,
            "erin",
            "Erin shares a <a href=\"file:///etc/hosts\">local file</a> instead.",
            json!([]),
        ),
        node(106, "frank", &long("Frank", 20), json!([])),
    ])
}

fn write_exe(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Wait for a condition that is not on screen (the fake's request log, the
/// opener's file).
fn eventually(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + LOAD;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Whether the detail pane is showing story `id`: its `thread` field is
/// drawn only there, and at the end of its row.
fn shows(text: &str, id: u64) -> bool {
    let want = format!("news.ycombinator.com/item?id={id}");
    text.lines().any(|l| l.trim_end().ends_with(&want))
}

// --------------------------------------------------------------- the terminal

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Probe {
    /// Answers DA1 only, as every terminal does.
    #[default]
    Da1,
    /// Also reports its colours (OSC 10/11): a dark theme.
    Colours,
    /// Answers nothing, so the probe has to time out.
    Silent,
    /// Reports its colours, but the reply is split in the middle of OSC 11
    /// and the rest arrives only after the probe has given up, as a slow
    /// link can deliver it.
    Split,
}

const DA1: &[u8] = b"\x1b[?62;22c";
/// A dark theme's colours, then DA1.
const COLOURS: &[u8] =
    b"\x1b]10;rgb:d4d4/d8d8/dede\x1b\\\x1b]11;rgb:1616/1818/1c1c\x1b\\\x1b[?62;22c";

struct Opts<'a> {
    env: &'a [(&'a str, &'a str)],
    probe: Probe,
    cols: u16,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            env: &[],
            probe: Probe::Da1,
            cols: COLS,
        }
    }
}

struct Term {
    screen: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Set once the reader has seen the end of hackney's output.
    eof: Arc<AtomicBool>,
    /// Set if the emulator itself panicked: the screen can't be trusted.
    broken: Arc<AtomicBool>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
}

impl Term {
    fn start(world: &World, opts: Opts) -> Term {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: opts.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hackney"));
        cmd.cwd(world.dir.path());
        // Hermetic: the fake for the network, the recorder for the browser,
        // and none of the developer's own environment leaking in.
        cmd.env("HOME", world.dir.path());
        cmd.env("TERM", "xterm-256color");
        cmd.env("HACKNEY_FIREBASE_URL", world.hn.firebase());
        cmd.env("HACKNEY_ALGOLIA_URL", world.hn.algolia());
        cmd.env("HACKNEY_OPENER", world.opener());
        cmd.env_remove("NO_COLOR");
        for (k, v) in opts.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let screen = Arc::new(Mutex::new(vt100::Parser::new(ROWS, opts.cols, 0)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let broken = Arc::new(AtomicBool::new(false));
        let input: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (screen2, raw2, input2, eof2, broken2, probe) = (
            screen.clone(),
            raw.clone(),
            input.clone(),
            eof.clone(),
            broken.clone(),
            opts.probe,
        );
        std::thread::spawn(move || {
            // Behave like a terminal: answer the colour probe, which ends
            // with a DA1 query. `held` is the part of the answer that only
            // arrives once hackney has stopped waiting for it.
            let (reply, mut held): (&[u8], &[u8]) = match probe {
                Probe::Da1 => (DA1, b""),
                Probe::Colours => (COLOURS, b""),
                Probe::Silent => (b"", b""),
                Probe::Split => {
                    COLOURS.split_at(COLOURS.windows(5).position(|w| w == b"/1c1c").unwrap())
                }
            };
            let has = |raw: &[u8], seq: &[u8]| raw.windows(seq.len()).any(|w| w == seq);
            // Output is handed to the emulator a whole frame at a time, so a
            // test never sees half a frame: a wait that passed on the top of
            // a frame would otherwise race the rest of it.
            let emulate = |bytes: &[u8]| {
                // Keep reading whatever happens to the emulator: a pty
                // nobody drains blocks hackney mid-frame.
                if !broken2.load(Ordering::SeqCst) {
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        lock(&screen2).process(bytes)
                    }));
                    broken2.store(ok.is_err(), Ordering::SeqCst);
                }
            };
            let mut pending = Vec::new();
            let mut buf = [0u8; 8192];
            let mut asked = false;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&buf[..n]);
                if let Some(end) = frame_end(&pending) {
                    emulate(&pending[..end]);
                    pending.drain(..end);
                }
                let mut raw = raw2.lock().unwrap();
                raw.extend_from_slice(&buf[..n]);
                if !asked && has(&raw, b"\x1b[c") {
                    asked = true;
                    let _ = input2.lock().unwrap().write_all(reply);
                }
                // On the alternate screen, the probe is over.
                if asked && !held.is_empty() && has(&raw, b"\x1b[?1049h") {
                    let _ = input2.lock().unwrap().write_all(held);
                    held = b"";
                }
            }
            emulate(&pending);
            eof2.store(true, Ordering::SeqCst);
        });
        Term {
            screen,
            raw,
            input,
            eof,
            broken,
            child,
            master: pair.master,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let parser = lock(&self.screen);
        assert!(
            !self.broken.load(Ordering::SeqCst),
            "the terminal emulator (vt100) panicked: the screen can't be trusted"
        );
        f(parser.screen())
    }

    fn text(&self) -> String {
        self.with(|s| s.contents())
    }

    fn row(&self, r: u16) -> String {
        self.with(|s| s.rows(0, s.size().1).nth(r as usize).unwrap_or_default())
    }

    /// Row `r` from column `from` on.
    fn row_from(&self, r: u16, from: u16) -> String {
        self.with(|s| {
            let cols = s.size().1;
            s.rows(from, cols.saturating_sub(from))
                .nth(r as usize)
                .unwrap_or_default()
        })
    }

    fn nav(&self) -> String {
        self.row(0)
    }

    fn rail(&self) -> String {
        self.row(2)
    }

    fn footer(&self) -> String {
        self.row(FOOTER)
    }

    /// Where `needle` is drawn in `rows`, at or right of column `from`: the
    /// row and column of its first cell. Columns come from the cells, so wide
    /// and multi-byte characters before it don't throw the column off.
    fn find_in(&self, needle: &str, rows: Range<u16>, from: u16) -> Option<(u16, u16)> {
        self.with(|s| {
            let cols = s.size().1;
            for r in rows {
                let mut line = String::new();
                let mut at = Vec::new();
                for c in from..cols {
                    let Some(cell) = s.cell(r, c) else { break };
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let text = match cell.contents() {
                        "" => " ",
                        t => t,
                    };
                    for ch in text.chars() {
                        line.push(ch);
                        at.push(c);
                    }
                }
                if let Some(i) = line.find(needle) {
                    return Some((r, at[line[..i].chars().count()]));
                }
            }
            None
        })
    }

    fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.find_in(needle, 0..ROWS, 0)
    }

    /// Wait until `needle` is drawn at or right of column `from`.
    fn wait_find(&self, needle: &str, from: u16) -> (u16, u16) {
        self.wait_until(&format!("{needle:?} from column {from}"), || {
            self.find_in(needle, 0..ROWS, from).is_some()
        });
        self.find_in(needle, 0..ROWS, from).unwrap()
    }

    /// The first column of the detail pane, side by side: just right of the
    /// divider that meets the rail at `┬`.
    fn detail_x(&self) -> u16 {
        self.find_in("┬", 2..3, 0)
            .map(|(_, c)| c + 1)
            .expect("side by side: a divider on the rail")
    }

    /// Wait until `f` holds; the screen at the deadline is in the failure
    /// message, so a failing test shows what was on screen.
    fn wait_until(&self, what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + LOAD;
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}. Screen:\n{}", self.text());
    }

    fn wait(&self, what: &str, f: impl Fn(&str) -> bool) {
        self.wait_until(what, || f(&self.text()));
    }

    fn wait_for(&self, needle: &str) {
        self.wait(&format!("{needle:?}"), |t| t.contains(needle));
    }

    fn wait_gone(&self, needle: &str) {
        self.wait(&format!("{needle:?} to go"), |t| !t.contains(needle));
    }

    fn wait_story(&self, id: u64) {
        self.wait(&format!("story {id} in the detail pane"), |t| shows(t, id));
    }

    fn send(&self, bytes: &[u8]) {
        if let Err(e) = self.input.lock().unwrap().write_all(bytes) {
            panic!("hackney is gone ({e}). Screen:\n{}", self.text());
        }
    }

    /// A left click at a 0-based cell: SGR press and release, 1-based.
    fn click(&self, at: (u16, u16)) {
        self.send(click_seq(at).as_bytes());
    }

    fn wheel(&self, (row, col): (u16, u16), down: bool) {
        let b = if down { 65 } else { 64 };
        self.send(format!("\x1b[<{b};{};{}M", col + 1, row + 1).as_bytes());
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn pid(&self) -> i32 {
        self.child.process_id().unwrap() as i32
    }

    fn wait_exit(&mut self, within: Duration) -> portable_pty::ExitStatus {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "hackney did not exit within {within:?}. Screen:\n{}",
            self.text()
        );
    }

    fn raw(&self) -> Vec<u8> {
        self.raw.lock().unwrap().clone()
    }

    fn resize(&self, rows: u16, cols: u16) {
        // The emulator first: a frame drawn for the new size must not be
        // wrapped at the old one.
        lock(&self.screen).screen_mut().set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }

    /// The terminal's line discipline: whether it is back to cooked mode,
    /// with line editing and echo.
    fn cooked(&self) -> bool {
        let fd = self.master.as_raw_fd().expect("the pty's fd");
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0, "tcgetattr");
        let want = libc::ICANON | libc::ECHO;
        t.c_lflag & want == want
    }

    /// After hackney has exited: the terminal is as it was found. Off the
    /// alternate screen, mouse reporting off, the cursor shown, cooked mode.
    fn assert_restored(&self) {
        self.wait_until("the end of hackney's output", || {
            self.eof.load(Ordering::SeqCst)
        });
        let raw = self.raw();
        let last = |seq: &[u8]| raw.windows(seq.len()).rposition(|w| w == seq);
        for (on, off, what) in [
            (
                &b"\x1b[?1049h"[..],
                &b"\x1b[?1049l"[..],
                "the alternate screen",
            ),
            (b"\x1b[?1000h", b"\x1b[?1000l", "mouse reporting"),
            (b"\x1b[?1006h", b"\x1b[?1006l", "SGR mouse reporting"),
        ] {
            let on = last(on).unwrap_or_else(|| panic!("{what} was never turned on"));
            let off = last(off).unwrap_or_else(|| panic!("{what} was never turned off"));
            assert!(off > on, "{what} was left on");
        }
        self.with(|s| {
            assert!(!s.alternate_screen(), "still on the alternate screen");
            assert_eq!(
                s.mouse_protocol_mode(),
                vt100::MouseProtocolMode::None,
                "mouse reporting left on"
            );
            assert!(!s.hide_cursor(), "cursor left hidden");
        });
        assert!(self.cooked(), "the terminal was left in raw mode");
    }
}

impl Drop for Term {
    /// Quit the way a user would and wait for it. Only a hackney that will
    /// not quit is killed.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.input.lock().map(|mut i| i.write_all(b"\x03"));
            let end = Instant::now() + Duration::from_secs(3);
            while Instant::now() < end {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Just past the last complete frame in `out`. ratatui ends every frame by
/// hiding the cursor, and hackney's exit by showing it.
fn frame_end(out: &[u8]) -> Option<usize> {
    out.windows(6)
        .rposition(|w| w == b"\x1b[?25l" || w == b"\x1b[?25h")
        .map(|i| i + 6)
}

/// The emulator, even after a panic inside it poisoned the lock.
fn lock(screen: &Mutex<vt100::Parser>) -> std::sync::MutexGuard<'_, vt100::Parser> {
    screen.lock().unwrap_or_else(|e| e.into_inner())
}

fn click_seq((row, col): (u16, u16)) -> String {
    format!("\x1b[<0;{c};{r}M\x1b[<0;{c};{r}m", c = col + 1, r = row + 1)
}

/// The standard world, started, with the first story's comments on screen.
fn started(w: &World) -> Term {
    let t = Term::start(w, Opts::default());
    t.wait_for("Alice opens the thread");
    t
}

fn is_rgb(c: vt100::Color) -> bool {
    matches!(c, vt100::Color::Rgb(..))
}

/// Every cell's foreground and background.
fn colours(t: &Term) -> Vec<vt100::Color> {
    t.with(|s| {
        let (rows, cols) = s.size();
        (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .filter_map(|(r, c)| s.cell(r, c))
            .flat_map(|cell| [cell.fgcolor(), cell.bgcolor()])
            .collect()
    })
}

// ------------------------------------------------------------ startup, keys

#[test]
fn startup_shows_the_front_page_and_the_first_storys_comments() {
    let w = World::standard();
    let t = started(&w);
    t.wait("every page loaded and counted", |_| {
        t.nav().contains("40 stories") && t.rail().contains(" 1/40 ")
    });
    assert!(
        t.nav().contains("hackney │ Top stories"),
        "nav: {}",
        t.nav()
    );
    let tabs = t.row(1);
    for tab in ["Top", "New", "Best", "Ask", "Show", "Jobs"] {
        assert!(tabs.contains(tab), "tab {tab} missing: {tabs}");
    }
    let text = t.text();
    for needle in [
        "Headline number 1 here",
        "Headline number 2 here",
        "Ask HN: How do you test a terminal app?",
        "6 comments",
        "▾ alice",
        "│ ▾ bob",
        "│ │ ▾ carol",
        "Carol goes one level deeper.",
        "article   example.com/1",
    ] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
    assert!(shows(&text, 1), "{text}");
    assert!(t.footer().contains("q quit"), "footer: {}", t.footer());
}

#[test]
fn j_and_k_and_the_arrows_move_the_selection() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"j");
    t.wait_story(2);
    t.wait_for("Story two has a single reply");
    assert!(t.rail().contains(" 2/"), "rail: {}", t.rail());
    assert!(t.row(4).starts_with("▎  2"), "row: {}", t.row(4));

    t.send(b"k");
    t.wait_story(1);
    t.wait_for("Alice opens the thread");

    t.send(b"\x1b[B"); // down
    t.wait_story(2);
    t.send(b"\x1b[A"); // up
    t.wait_story(1);
}

#[test]
fn tab_and_number_keys_switch_feeds() {
    let w = World::standard();
    let t = started(&w);

    t.send(b"\t");
    t.wait("the New feed", |s| {
        t.nav().contains("New stories") && shows(s, 501) && s.contains("Another new arrival")
    });

    // Best is not on the fake: its tab says why rather than hanging.
    t.send(b"\t");
    t.wait("Best's failure", |_| {
        t.nav().contains("Best stories") && t.footer().contains("beststories")
    });

    t.send(b"6");
    t.wait("the Jobs feed", |s| {
        t.nav().contains("Jobs") && shows(s, 601) && s.contains("Example Corp is hiring")
    });

    t.send(b"\x1b[Z"); // shift-tab
    t.wait("the Show feed", |_| t.nav().contains("Show HN"));

    t.send(b"1");
    t.wait("the Top feed", |s| {
        t.nav().contains("Top stories") && shows(s, 1) && s.contains("Alice opens the thread")
    });
}

#[test]
fn switching_back_to_a_feed_is_instant_and_fetches_nothing() {
    let w = World::standard();
    let t = started(&w);
    assert_eq!(w.hn.hits("/v0/topstories.json"), 1);
    // Were Top fetched again, it would be "loading" for all of three seconds.
    w.hn.delay("/v0/topstories.json", Duration::from_secs(3));

    t.send(b"2");
    t.wait_story(501);
    t.send(b"1");
    t.wait("Top, with its comments", |s| {
        t.nav().contains("Top stories") && shows(s, 1) && s.contains("Alice opens the thread")
    });
    assert!(!t.nav().contains("loading"), "nav: {}", t.nav());

    // Something after it, so a request it started would have been made.
    t.send(b"j");
    t.wait_story(2);
    assert_eq!(w.hn.hits("/v0/topstories.json"), 1, "{:?}", w.hn.requests());
    assert_eq!(w.hn.hits("/v0/newstories.json"), 1, "{:?}", w.hn.requests());
}

#[test]
fn comments_are_fetched_ahead_but_not_for_rows_the_cursor_runs_past() {
    let w = World::standard();
    let t = started(&w);
    let tree = |id: u64| w.hn.hits(&format!("/algolia/v1/items/{id}"));
    eventually("the next three stories fetched ahead", || {
        (2..=4).all(|id| tree(id) == 1)
    });

    // Down ten rows in one go: only where the cursor comes to rest counts.
    t.send(b"jjjjjjjjjj");
    t.wait_story(11);
    eventually("comments around story 11", || {
        [10, 11, 12, 13, 14].iter().all(|&id| tree(id) == 1)
    });
    for id in 5..=9 {
        assert_eq!(tree(id), 0, "fetched story {id}'s comments while passing");
    }
    assert_eq!(tree(1), 1, "story 1 fetched twice");
    assert_eq!(tree(2), 1, "story 2 fetched again though fresh");
}

#[test]
fn the_next_page_loads_near_the_end_and_a_failed_page_is_asked_for_again() {
    let w = World::new();
    let ids: Vec<u64> = (1..=100).collect();
    w.hn.feed("topstories", &ids);
    // Stories 61-90 are missing, so the third page fails outright.
    for id in (1..=60).chain(91..=100) {
        w.hn.story(id, &headline(id), &[]);
    }
    let t = Term::start(&w, Opts::default());
    // The first page doesn't fill the pane's lookahead, so a second follows.
    t.wait("two pages", |_| t.nav().contains("60 of 100"));

    t.send(b"G");
    t.wait("the failed page", |_| t.footer().contains("item/61.json"));
    assert!(t.nav().contains("60 of 100"), "nav: {}", t.nav());

    for id in 61..=90 {
        w.hn.story(id, &headline(id), &[]);
    }
    t.send(b"k");
    t.wait("the page, asked for again", |_| {
        t.nav().contains("90 of 100")
    });
    assert!(!t.footer().contains('✗'), "footer: {}", t.footer());

    t.send(b"G");
    t.wait("the last page", |_| t.nav().contains("100 stories"));
    t.send(b"G");
    t.wait_story(100);
}

#[test]
fn search_opens_a_search_tab_with_the_results() {
    let w = World::standard();
    w.hn.search(json!([
        {"objectID": "901", "title": "Rust in the terminal", "url": "https://example.com/901",
         "author": "octocat", "points": 77, "num_comments": 0, "created_at_i": now() - 100},
        {"objectID": "902", "title": "Another rust story", "url": "https://example.com/902",
         "author": "octocat", "points": 12, "num_comments": 0, "created_at_i": now() - 200},
    ]));
    let t = started(&w);

    t.send(b"/");
    t.wait("the prompt", |_| t.footer().contains("esc cancel"));
    t.send(b"rust");
    t.wait("the query typed", |_| t.footer().contains("/rust▏"));
    t.send(b"\r");
    t.wait("the results", |s| {
        t.nav().contains("Search  “rust”") && shows(s, 901) && s.contains("Another rust story")
    });
    assert!(t.row(1).contains("⌕ rust"), "tabs: {}", t.row(1));
    assert!(t.nav().contains("2 stories"), "nav: {}", t.nav());
    let searches: Vec<String> =
        w.hn.requests()
            .into_iter()
            .filter(|r| r.starts_with("/algolia/v1/search?"))
            .collect();
    assert_eq!(searches.len(), 1, "{searches:?}");
    assert!(searches[0].contains("query=rust"), "{searches:?}");

    // The search is a tab like the others, and the seventh.
    t.send(b"1");
    t.wait_story(1);
    t.send(b"7");
    t.wait_story(901);
    t.send(b"1");
    t.wait_story(1);
    t.send(b"\x1b[Z"); // shift-tab from the first tab wraps to the last
    t.wait_story(901);
    assert_eq!(w.hn.hits("/algolia/v1/search"), 1, "searched again");
}

#[test]
fn the_search_prompt_takes_every_key_until_enter_or_esc() {
    let w = World::standard();
    let mut t = started(&w);

    t.send(b"/");
    t.wait("the prompt", |_| t.footer().contains("esc cancel"));
    t.send(b"qj");
    t.wait("typed, not obeyed", |_| t.footer().contains("/qj▏"));
    assert!(t.alive(), "q in the prompt quit");
    assert!(shows(&t.text(), 1), "j in the prompt moved the cursor");

    t.send(b"\x7f\x7f"); // backspace
    t.wait("an empty prompt", |_| t.footer().contains(" /▏"));
    t.send(b"\r");
    t.wait("the prompt closed", |_| t.footer().contains("? help"));

    t.send(b"/");
    t.wait("the prompt", |_| t.footer().contains("esc cancel"));
    t.send(b"abc");
    t.wait("typed", |_| t.footer().contains("/abc▏"));
    t.send(b"\x1b");
    t.wait("the prompt cancelled", |_| t.footer().contains("? help"));

    // Neither an empty query nor a cancelled one searched.
    t.send(b"j");
    t.wait_story(2);
    assert_eq!(w.hn.hits("/algolia/v1/search"), 0, "{:?}", w.hn.requests());
    assert!(!t.row(1).contains('⌕'), "tabs: {}", t.row(1));
}

#[test]
fn help_opens_holds_the_keys_and_closes() {
    let w = World::standard();
    let mut t = started(&w);
    let help = "move through stories";

    t.send(b"?");
    t.wait_for(help);
    // Under the help, keys and the wheel do nothing.
    t.send(b"j");
    t.wheel((10, 100), true);
    t.send(b"?");
    t.wait_gone(help);
    assert!(shows(&t.text(), 1), "j moved the cursor under the help");
    let x = t.detail_x();
    assert!(
        t.row_from(3, x).contains("Headline number 1 here"),
        "the wheel scrolled under the help: {}",
        t.row_from(3, x)
    );

    // q closes the help rather than quitting.
    t.send(b"?");
    t.wait_for(help);
    t.send(b"q");
    t.wait_gone(help);
    assert!(t.alive(), "q quit from the help");

    t.send(b"?");
    t.wait_for(help);
    t.send(b"\x1b");
    t.wait_gone(help);

    // A click anywhere closes it, and does nothing else.
    t.send(b"?");
    t.wait_for(help);
    t.click((10, 20));
    t.wait_gone(help);
    assert!(shows(&t.text(), 1), "the click went through the help");
}

#[test]
fn z_zooms_the_comments_to_the_whole_width_and_back() {
    let w = World::standard();
    let t = started(&w);

    t.send(b"z");
    t.wait("zoomed", |s| {
        t.footer().contains("z unzoom") && !s.contains("Headline number 2 here")
    });
    assert!(!t.rail().contains('┬'), "rail: {}", t.rail());
    let (_, col) = t.find("Headline number 1 here").unwrap();
    assert!(
        (25..40).contains(&col),
        "the title should be centred, is at column {col}"
    );

    t.send(b"z");
    t.wait("unzoomed", |s| {
        s.contains("Headline number 2 here") && t.rail().contains('┬')
    });

    t.send(b"z");
    t.wait("zoomed", |_| t.footer().contains("z unzoom"));
    t.send(b"\x1b");
    t.wait("esc unzooms", |s| s.contains("Headline number 2 here"));
}

#[test]
fn n_and_p_jump_between_threads_and_x_folds_the_one_being_read() {
    let w = World::standard();
    let t = started(&w);
    let x = t.detail_x();
    let top = || t.row_from(3, x);

    t.send(b"n");
    t.wait("alice's thread at the top", |_| top().contains("▾ alice"));
    t.send(b"n");
    t.wait("dave's thread at the top", |_| top().contains("▾ dave"));
    t.send(b"p");
    t.wait("back to alice's", |_| top().contains("▾ alice"));

    t.send(b"x");
    t.wait("alice's thread folded", |_| {
        top().contains("▸ alice") && top().contains("+2")
    });
    let text = t.text();
    assert!(!text.contains("Bob answers alice"), "{text}");
    assert!(!text.contains("Alice opens the thread"), "{text}");

    t.send(b"x");
    t.wait("unfolded", |s| {
        top().contains("▾ alice") && s.contains("Bob answers alice directly.")
    });
}

// ------------------------------------------------------------------- mouse

#[test]
fn the_wheel_over_the_list_scrolls_it_without_moving_the_cursor() {
    let w = World::standard();
    let t = started(&w);
    t.wait("all forty", |_| t.rail().contains(" 1/40 "));

    t.wheel((10, 20), true);
    t.wait("the list scrolled", |_| t.row(3).starts_with("   4  ▲"));
    let text = t.text();
    assert!(shows(&text, 1), "the wheel moved the cursor:\n{text}");
    assert!(text.contains("Alice opens the thread"), "{text}");
    assert!(t.rail().contains(" 1/40 "), "rail: {}", t.rail());
    assert!(t.find("▎  1").is_none(), "{text}");

    // A flick goes no further than the last story.
    t.send("\x1b[<65;21;11M".repeat(20).as_bytes());
    t.wait("the list at its end", |_| {
        t.row(3).starts_with("  15  ▲") && t.row(FOOTER - 1).starts_with("  40  ▲")
    });
    assert!(shows(&t.text(), 1), "{}", t.text());

    // The keys bring the view back to the cursor.
    t.send(b"j");
    t.wait("the view back on the cursor", |s| {
        t.row(3).starts_with("▎  2") && shows(s, 2)
    });
}

#[test]
fn the_wheel_over_the_comments_scrolls_them() {
    let w = World::standard();
    let t = started(&w);
    let x = t.detail_x();

    t.wheel((10, x + 20), true);
    t.wait("the comments scrolled", |_| {
        t.row_from(3, x).contains("article   example.com/1")
    });
    t.wheel((10, x + 20), false);
    t.wait("scrolled back", |_| {
        t.row_from(3, x).contains("Headline number 1 here")
    });
    assert!(shows(&t.text(), 1));
}

#[test]
fn a_click_on_a_story_selects_it() {
    let w = World::standard();
    let t = started(&w);
    t.click((5, 30));
    t.wait_story(3);
    t.wait_for("I drive the real binary in a pty.");
    assert!(t.rail().contains(" 3/"), "rail: {}", t.rail());
    assert!(w.opened().is_empty(), "a single click opened something");
}

#[test]
fn a_double_click_opens_the_article_and_a_third_click_does_not_open_it_again() {
    let w = World::standard();
    let t = started(&w);

    t.send(format!("{}{}", click_seq((4, 30)), click_seq((4, 30))).as_bytes());
    assert_eq!(w.wait_opened(1), ["https://example.com/2"]);
    t.wait_story(2);

    // A triple click on story 5, then a double click on story 6: had the
    // third click opened story 5 again, it would be in before story 6.
    t.send(format!("{0}{0}{0}", click_seq((7, 30))).as_bytes());
    t.send(format!("{0}{0}", click_seq((8, 30))).as_bytes());
    eventually("story 6 opened", || {
        w.opened().iter().any(|u| u == "https://example.com/6")
    });
    let mut opened = w.opened();
    opened.sort();
    assert_eq!(
        opened,
        [
            "https://example.com/2",
            "https://example.com/5",
            "https://example.com/6"
        ]
    );
}

#[test]
fn a_click_on_a_tab_switches_feed() {
    let w = World::standard();
    let t = started(&w);
    let (row, col) = t.find_in("New", 1..2, 0).expect("the New tab");
    t.click((row, col + 1));
    t.wait("the New feed", |s| {
        t.nav().contains("New stories") && shows(s, 501)
    });
    // The tab's stretch of rail under it is part of the target.
    let (row, col) = t.find_in("Jobs", 1..2, 0).expect("the Jobs tab");
    t.click((row + 1, col + 1));
    t.wait("the Jobs feed", |s| {
        t.nav().contains("Jobs") && shows(s, 601)
    });
}

#[test]
fn a_click_on_a_comment_header_folds_it() {
    let w = World::standard();
    let t = started(&w);
    let x = t.detail_x();
    let at = t.wait_find("▾ alice", x);
    t.click(at);
    t.wait("alice's thread folded", |s| {
        t.find("▸ alice")
            .is_some_and(|(r, _)| t.row(r).contains("+2"))
            && !s.contains("Carol goes one level deeper.")
    });
    // Folding closes the gap: dave moves up to where bob was.
    assert!(!t.text().contains("▾ bob"), "{}", t.text());

    let at = t.wait_find("▸ alice", x);
    t.click(at);
    t.wait_for("Carol goes one level deeper.");
    assert!(t.find("+2").is_none(), "{}", t.text());
}

#[test]
fn a_click_on_a_link_opens_it() {
    let w = World::standard();
    let t = started(&w);
    let x = t.detail_x();

    // The article field and the title are links; the title's twin in the
    // list is not.
    let at = t.find_in("example.com/1", 3..FOOTER, x + 2).unwrap();
    t.click(at);
    assert_eq!(w.wait_opened(1), ["https://example.com/1"]);
    let at = t.find_in("Headline number 1 here", 3..FOOTER, x).unwrap();
    t.click(at);
    assert_eq!(w.wait_opened(2)[1], "https://example.com/1");
    let at = t
        .find_in("news.ycombinator.com/item?id=1", 3..FOOTER, x)
        .unwrap();
    t.click(at);
    assert_eq!(
        w.wait_opened(3)[2],
        "https://news.ycombinator.com/item?id=1"
    );

    // A link in a comment, brought into view with `n`.
    t.send(b"nn");
    t.wait("dave's thread at the top", |_| {
        t.row_from(3, x).contains("▾ dave")
    });
    let at = t.wait_find("https://example.com/docs", x);
    t.click((at.0, at.1 + 5));
    assert_eq!(w.wait_opened(4)[3], "https://example.com/docs");
    t.wait("the notice", |_| {
        t.footer().contains("opened https://example.com/docs")
    });
    // The words around it are not the link.
    let at = t.find_in("Dave points at", 3..FOOTER, x).unwrap();
    t.click(at);
    t.send(b"j");
    t.wait_story(2);
    assert_eq!(w.opened().len(), 4, "{:?}", w.opened());
}

/// A click sent right behind a wheel notch is aimed at the frame the notch
/// produces, not at the one before it.
#[test]
fn a_click_behind_a_scroll_lands_on_the_row_scrolled_into_place() {
    let w = World::standard();
    let t = started(&w);
    t.wait("all forty", |_| t.rail().contains(" 1/40 "));
    let mut bytes = b"\x1b[<65;21;11M".to_vec();
    bytes.extend_from_slice(click_seq((3, 30)).as_bytes());
    t.send(&bytes);
    t.wait_story(4);
    assert!(t.row(3).starts_with("▎  4"), "row: {}", t.row(3));
}

// ----------------------------------------------------------------- opening

#[test]
fn o_and_enter_open_the_article() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"o");
    assert_eq!(w.wait_opened(1), ["https://example.com/1"]);
    t.wait("the notice", |_| {
        t.footer().contains("opened https://example.com/1")
    });
    t.send(b"j");
    t.wait_story(2);
    t.send(b"\r");
    assert_eq!(
        w.wait_opened(2),
        ["https://example.com/1", "https://example.com/2"]
    );
}

#[test]
fn c_opens_the_discussion() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"c");
    assert_eq!(w.wait_opened(1), ["https://news.ycombinator.com/item?id=1"]);
}

#[test]
fn a_text_post_shows_its_text_and_opens_its_discussion() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"jj");
    t.wait_story(3);
    t.wait_for("I drive the real binary in a pty.");
    assert!(!t.text().contains("article "), "a text post has no article");
    t.send(b"o");
    assert_eq!(w.wait_opened(1), ["https://news.ycombinator.com/item?id=3"]);
}

#[test]
fn links_that_are_not_web_links_are_refused_with_a_notice() {
    let w = World::standard();
    w.hn.feed("topstories", &[1, 7]);
    w.hn.item(json!({
        "id": 7, "type": "story", "by": "octocat", "title": "A story that points at a file",
        "url": "file:///etc/passwd", "score": 1, "time": now() - 60, "kids": [],
    }));
    let t = started(&w);
    let x = t.detail_x();
    t.send(b"nn");
    t.wait("dave's thread at the top", |_| {
        t.row_from(3, x).contains("▾ dave")
    });

    let at = t.wait_find("local file", x);
    t.click(at);
    t.wait("the refusal", |_| {
        t.footer()
            .contains("not opening file:///etc/hosts: not a web link")
    });

    t.send(b"j");
    t.wait_story(7);
    t.send(b"o");
    t.wait("the refusal", |_| {
        t.footer()
            .contains("not opening file:///etc/passwd: not a web link")
    });
    assert!(w.opened().is_empty(), "{:?}", w.opened());
}

// -------------------------------------------------------------- resilience

#[test]
fn a_failed_feed_shows_why_and_r_recovers() {
    let w = World::standard();
    w.hn.route("/v0/topstories.json", 500, "upstream on fire");
    let t = Term::start(&w, Opts::default());
    t.wait("the failure", |_| {
        t.footer().contains("✗") && t.footer().contains("topstories.json")
    });
    assert!(t.nav().contains("failed"), "nav: {}", t.nav());
    assert!(t.row(3).contains("✗"), "the list says so too: {}", t.row(3));

    w.hn.feed("topstories", &(1..=40).collect::<Vec<_>>());
    t.send(b"r");
    t.wait_for("Alice opens the thread");
    assert!(!t.footer().contains('✗'), "footer: {}", t.footer());
    assert!(!t.nav().contains("failed"), "nav: {}", t.nav());
}

#[test]
fn slow_comments_show_loading_then_arrive() {
    let w = World::standard();
    w.hn.delay("/algolia/v1/items/1", Duration::from_millis(1500));
    let t = Term::start(&w, Opts::default());
    t.wait("the list, and comments on the way", |s| {
        shows(s, 1) && s.contains("loading comments")
    });
    t.wait_for("Alice opens the thread");
    assert!(!t.text().contains("loading comments"), "{}", t.text());
}

#[test]
fn quitting_while_the_feed_hangs_is_prompt() {
    let w = World::standard();
    w.hn.delay("/v0/topstories.json", Duration::from_secs(60));
    let mut t = Term::start(&w, Opts::default());
    t.wait("the list loading", |_| t.row(3).contains("loading…"));
    t.send(b"q");
    let status = t.wait_exit(Duration::from_secs(2));
    assert!(status.success(), "{status:?}");
    t.assert_restored();
}

#[test]
fn comments_come_from_firebase_when_algolia_has_no_tree() {
    let w = World::new();
    w.hn.feed("topstories", &[1]);
    w.hn.story(1, "Too new for Algolia", &[11, 12, 13]);
    w.hn.comment(11, "alice", "Straight from Firebase.", &[14]);
    w.hn.comment(14, "bob", "A reply fetched a level later.", &[]);
    w.hn.item(json!({
        "id": 12, "type": "comment", "by": "spammer", "text": "Dead and gone.",
        "time": now() - 300, "dead": true,
    }));
    w.hn.item(json!({
        "id": 13, "type": "comment", "deleted": true, "time": now() - 300, "kids": [15],
    }));
    w.hn.comment(15, "carol", "Still here under a deleted comment.", &[]);

    let t = Term::start(&w, Opts::default());
    t.wait_for("Still here under a deleted comment.");
    let text = t.text();
    for needle in [
        "▾ alice",
        "Straight from Firebase.",
        "│ ▾ bob",
        "A reply fetched a level later.",
        "▾ [deleted]",
        "│ ▾ carol",
    ] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
    assert!(!text.contains("Dead and gone."), "{text}");
    assert_eq!(w.hn.hits("/algolia/v1/items/1"), 1, "Algolia was asked");
    let row = |needle| t.find(needle).unwrap().0;
    assert!(row("▾ alice") < row("▾ [deleted]"), "{text}");
}

#[test]
fn top_level_comments_follow_hns_ranking_not_algolias_posting_order() {
    let w = World::new();
    w.hn.feed("topstories", &[1]);
    w.hn.story(1, "A ranked thread", &[31, 32, 33]);
    // Algolia lists them as posted: the one HN ranks third came first.
    w.hn.tree(
        1,
        json!([
            node(33, "monalisa", "Ranked third by HN.", json!([])),
            node(31, "octocat", "Ranked first by HN.", json!([])),
            node(32, "hubot", "Ranked second by HN.", json!([])),
        ]),
    );
    let t = Term::start(&w, Opts::default());
    t.wait_for("Ranked third by HN.");
    let row = |needle| t.find(needle).unwrap().0;
    assert!(
        row("Ranked first") < row("Ranked second") && row("Ranked second") < row("Ranked third"),
        "{}",
        t.text()
    );
}

#[test]
fn comments_that_fail_to_load_say_why_and_r_retries() {
    let w = World::standard();
    // A search hit that Firebase doesn't know yet: its comments can't load.
    w.hn.search(json!([
        {"objectID": "901", "title": "Rust in the terminal", "url": "https://example.com/901",
         "author": "octocat", "points": 77, "num_comments": 1, "created_at_i": now() - 100},
    ]));
    let t = started(&w);
    t.send(b"/rust\r");
    t.wait("the comments' failure", |s| {
        shows(s, 901) && s.contains("✗") && s.contains("item/901.json")
    });
    assert!(!t.footer().contains('✗'), "not the feed's failure");

    w.hn.story(901, "Rust in the terminal", &[911]);
    w.hn.tree(
        901,
        json!([node(911, "octocat", "Found after a retry.", json!([]))]),
    );
    t.send(b"r");
    t.wait_for("Found after a retry.");
    assert!(!t.text().contains("item/901.json"), "{}", t.text());
}

#[test]
fn dead_deleted_missing_and_failed_stories_are_left_out() {
    let w = World::new();
    w.hn.feed("topstories", &[1, 2, 3, 4, 5, 6]);
    w.hn.story(1, &headline(1), &[]);
    let mut dead = story_json(2, "A dead story", &[]);
    dead["dead"] = json!(true);
    w.hn.item(dead);
    w.hn.route("/v0/item/3.json", 200, "null");
    w.hn.route("/v0/item/4.json", 500, "boom");
    let mut deleted = story_json(5, "A deleted story", &[]);
    deleted["deleted"] = json!(true);
    w.hn.item(deleted);
    w.hn.story(6, &headline(6), &[]);

    let t = Term::start(&w, Opts::default());
    t.wait("the stories that exist", |s| {
        s.contains("Headline number 1 here") && s.contains("Headline number 6 here")
    });
    let text = t.text();
    assert!(!text.contains("A dead story"), "{text}");
    assert!(!text.contains("A deleted story"), "{text}");
    assert!(!t.footer().contains('✗'), "one failed item failed the page");
    assert!(t.row(4).contains("Headline number 6 here"), "{text}");
    assert!(!t.row(5).contains('▲'), "{text}");
}

#[test]
fn control_characters_from_the_api_never_reach_the_terminal() {
    let w = World::new();
    w.hn.feed("topstories", &[1, 2]);
    w.hn.item(json!({
        "id": 1, "type": "story", "by": "octo\u{1b}[31mcat",
        "title": "Evil\u{1b}]0;pwned\u{7}\u{1b}[2J\ttitle",
        "url": "https://example.com/1", "score": 101, "time": now() - 3600, "kids": [101],
    }));
    w.hn.story(2, &headline(2), &[]);
    w.hn.tree(
        1,
        json!([node(
            101,
            "octocat",
            "Comment\u{1b}]0;pwned-too\u{7} with\u{1b}[5m escapes",
            json!([])
        )]),
    );
    let t = Term::start(&w, Opts::default());
    t.wait_for("with[5m escapes");

    let raw = t.raw();
    let has = |seq: &[u8]| raw.windows(seq.len()).any(|w| w == seq);
    assert!(
        !has(b"\x1b]0;pwned"),
        "an OSC from the API reached the terminal"
    );
    assert!(!has(b"\x1b[5m"), "an SGR from the API reached the terminal");
    let text = t.text();
    assert!(text.contains("Evil]0;pwned[2Jtitle"), "{text}");
    assert!(
        text.contains("hackney │ Top stories"),
        "the screen was cleared"
    );
    // Every row keeps its columns: the age sits in the same place on both.
    let age = |r: u16| t.find_in(" 1h ", r..r + 1, 0).map(|(_, c)| c);
    assert!(age(3).is_some() && age(3) == age(4), "{text}");
    let divider = |r: u16| t.find_in("│", r..r + 1, 40).map(|(_, c)| c);
    assert_eq!(divider(3), divider(4), "{text}");
}

#[test]
fn a_very_deep_thread_neither_crashes_nor_stalls() {
    const DEPTH: u64 = 1500;
    let w = World::new();
    w.hn.feed("topstories", &[1, 2]);
    w.hn.story(1, "A very deep thread", &[1000]);
    w.hn.story(2, &headline(2), &[]);
    // Built as a string: a `Value` this deep would recurse in the test too.
    let mut tree = r#"{"id":1,"author":"octocat","children":["#.to_string();
    for i in 0..DEPTH {
        tree.push_str(&format!(
            r#"{{"id":{},"author":"u{i}","text":"level {i} of the chain","created_at_i":{},"children":["#,
            1000 + i,
            now() - 600
        ));
    }
    for _ in 0..=DEPTH {
        tree.push_str("]}");
    }
    w.hn.route("/algolia/v1/items/1", 200, tree);

    let mut t = Term::start(&w, Opts::default());
    t.wait_for("level 0 of the chain");
    // To the bottom of the thread, a page at a time.
    t.send(&[b' '; 400]);
    t.wait_for(&format!("level {} of the chain", DEPTH - 1));
    t.send(b"j");
    t.wait_story(2);
    assert!(t.alive());
}

#[test]
fn r_refreshes_behind_the_list_on_screen_and_keeps_the_cursor_on_its_story() {
    let w = World::standard();
    let t = started(&w);
    t.wait("all forty", |_| t.rail().contains(" 1/40 "));

    // A story arrives on top while the refresh is slow.
    w.hn.story(41, "A story that just arrived", &[]);
    w.hn.feed(
        "topstories",
        &[41].into_iter().chain(1..=40).collect::<Vec<_>>(),
    );
    w.hn.delay("/v0/topstories.json", Duration::from_millis(1500));
    t.send(b"r");
    t.wait("the refresh under way", |_| t.nav().contains("loading"));
    let text = t.text();
    assert!(
        text.contains("Headline number 1 here") && shows(&text, 1),
        "the list left the screen while refreshing:\n{text}"
    );

    t.wait_for("A story that just arrived");
    t.wait("the cursor still on story 1", |s| {
        shows(s, 1) && t.rail().contains(" 2/40 ")
    });
}

// ---------------------------------------------------- terminal hygiene

fn exits_cleanly(how: impl FnOnce(&Term)) {
    let w = World::standard();
    let mut t = started(&w);
    assert!(!t.cooked(), "hackney should run in raw mode");
    t.with(|s| {
        assert!(s.alternate_screen());
        assert_ne!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
    });
    how(&t);
    let status = t.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "{status:?}");
    t.assert_restored();
}

#[test]
fn q_exits_and_restores_the_terminal() {
    exits_cleanly(|t| t.send(b"q"));
}

#[test]
fn ctrl_c_exits_and_restores_the_terminal() {
    exits_cleanly(|t| t.send(b"\x03"));
}

#[test]
fn ctrl_c_quits_even_mid_search() {
    exits_cleanly(|t| {
        t.send(b"/");
        t.wait("the prompt", |_| t.footer().contains("esc cancel"));
        t.send(b"half a query");
        t.wait("typed", |_| t.footer().contains("half a query"));
        t.send(b"\x03");
    });
}

#[test]
fn sigterm_exits_and_restores_the_terminal() {
    exits_cleanly(|t| unsafe {
        libc::kill(t.pid(), libc::SIGTERM);
    });
}

#[test]
fn sighup_exits_and_restores_the_terminal() {
    exits_cleanly(|t| unsafe {
        libc::kill(t.pid(), libc::SIGHUP);
    });
}

#[test]
fn resizing_switches_between_side_by_side_and_stacked() {
    let w = World::standard();
    let t = started(&w);
    assert!(t.rail().contains('┬'), "rail: {}", t.rail());
    assert!(t.row(10).contains('│'), "no divider: {}", t.row(10));

    t.resize(ROWS, 80);
    // Stacked, the comments' pane has a rule across the whole width on top.
    let rule = "─".repeat(80);
    t.wait("the stacked layout", |_| {
        !t.rail().contains('┬') && (3..FOOTER).any(|r| t.row(r) == rule)
    });
    let text = t.text();
    assert!(shows(&text, 1), "{text}");
    assert!(text.contains("Headline number 1 here"), "{text}");
    assert!(t.footer().contains("q quit"), "footer: {}", t.footer());

    t.resize(ROWS, COLS);
    t.wait("side by side again", |_| t.rail().contains('┬'));
    t.wait_for("Alice opens the thread");
}

// ------------------------------------------------------------ colour probe

fn started_with(w: &World, opts: Opts) -> Term {
    let t = Term::start(w, opts);
    t.wait_for("Alice opens the thread");
    t
}

#[test]
fn the_selection_band_is_mixed_from_the_reported_colours() {
    let w = World::standard();
    let t = started_with(
        &w,
        Opts {
            probe: Probe::Colours,
            ..Opts::default()
        },
    );
    let (row, _) = t.find("▎  1").unwrap();
    let band = t.with(|s| s.cell(row, 30).unwrap().bgcolor());
    let vt100::Color::Rgb(r, g, b) = band else {
        panic!("no band from the reported colours: {band:?}");
    };
    // Between the reported background (16 18 1c) and foreground (d4 d8 de),
    // and close to the background.
    assert!(
        (0x17..0x60).contains(&r) && (0x19..0x60).contains(&g) && (0x1d..0x60).contains(&b),
        "{band:?}"
    );
    let other = t.with(|s| s.cell(row + 1, 30).unwrap().bgcolor());
    assert_eq!(other, vt100::Color::Default, "the band spilled over");
}

#[test]
fn a_terminal_that_reports_no_colours_gets_no_rgb() {
    for probe in [Probe::Da1, Probe::Silent] {
        let w = World::standard();
        let t = started_with(
            &w,
            Opts {
                probe,
                ..Opts::default()
            },
        );
        assert!(t.find("▎  1").is_some());
        assert!(!colours(&t).into_iter().any(is_rgb), "{}", t.text());
    }
}

/// Two layers keep this true: hackney's own theme, and crossterm, which drops
/// colour under NO_COLOR by itself. The test fails only if both give way.
#[test]
fn no_color_draws_in_the_terminals_own_colours_only() {
    let w = World::standard();
    let t = started_with(
        &w,
        Opts {
            env: &[("NO_COLOR", "1")],
            probe: Probe::Colours,
            ..Opts::default()
        },
    );
    assert!(t.find("▎  1").is_some());
    let odd: Vec<_> = colours(&t)
        .into_iter()
        .filter(|c| *c != vt100::Color::Default)
        .collect();
    assert!(
        odd.is_empty(),
        "colours under NO_COLOR: {:?}",
        &odd[..odd.len().min(5)]
    );
}

/// A colour reply that turns up after the probe stopped waiting arrives as
/// input. Read as keys, `11;rgb:6666/…` would refresh (`r`), switch to Jobs
/// (`6`) and open the search prompt (`/`).
#[test]
fn a_late_colour_reply_is_not_read_as_keys() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"\x1b]11;rgb:6666/6666/6666\x1b\\");
    t.send(b"\x1b]10;rgb:6666/6666/6666\x07"); // BEL-terminated
    t.send(b"j");
    t.wait_story(2);
    assert!(t.nav().contains("Top stories"), "nav: {}", t.nav());
    assert!(!t.footer().contains("esc cancel"), "footer: {}", t.footer());
    assert_eq!(w.hn.hits("/v0/jobstories.json"), 0);
    assert_eq!(w.hn.hits("/v0/topstories.json"), 1, "refreshed");

    // And the keys that follow it are keys again.
    t.send(b"6");
    t.wait("the Jobs feed", |s| {
        t.nav().contains("Jobs") && shows(s, 601)
    });
}

#[test]
fn a_late_colour_reply_cut_short_stops_swallowing_keys() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"\x1b]11;rgb:6666/66");
    // Past the window in which hackney waits for the reply's end.
    std::thread::sleep(Duration::from_millis(800));
    t.send(b"j");
    t.wait_story(2);
    assert!(t.nav().contains("Top stories"), "nav: {}", t.nav());
    assert_eq!(w.hn.hits("/v0/jobstories.json"), 0);
    t.send(b"6");
    t.wait("the Jobs feed", |s| {
        t.nav().contains("Jobs") && shows(s, 601)
    });
}

// ------------------------------------------------------------------- more

/// A query string's value for `key`, decoded as a form would encode it.
fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    let raw = query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))?;
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                out.push(u8::from_str_radix(raw.get(i + 1..i + 3)?, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[test]
fn g_end_and_the_page_keys_move_the_selection() {
    let w = World::standard();
    let t = started(&w);
    t.wait("all forty", |_| t.rail().contains(" 1/40 "));
    for (key, id) in [
        (&b"\x1b[6~"[..], 11), // page down
        (b"\x1b[5~", 1),       // page up
        (b"G", 40),
        (b"g", 1),
        (b"\x1b[F", 40), // end
        (b"\x1b[H", 1),  // home
    ] {
        t.send(key);
        t.wait_story(id);
    }
}

#[test]
fn shift_j_and_k_space_and_b_scroll_the_comments() {
    let w = World::standard();
    let t = started(&w);
    let x = t.detail_x();
    let line = |r: u16| t.row_from(r, x);

    t.send(b"J");
    t.wait("a line down", |_| line(3).contains("▲ 101 · by octocat"));
    t.send(b"K");
    t.wait("a line up", |_| line(3).contains("Headline number 1 here"));
    // A page is the pane less two lines, which brings dave's header up to
    // the pane's second row.
    t.send(b" ");
    t.wait("a page down", |_| line(4).contains("▾ dave"));
    t.send(b"b");
    t.wait("a page up", |_| line(3).contains("Headline number 1 here"));
}

#[test]
fn with_no_input_the_clock_keeps_time_and_a_notice_goes_away() {
    let w = World::standard();
    let t = started(&w);
    t.send(b"o");
    t.wait("the notice", |_| {
        t.footer().contains("opened https://example.com/1")
    });
    // Nothing is sent from here on: only hackney's own timer moves these.
    t.wait("the clock to move", |_| {
        (3..=9).any(|n| t.nav().contains(&format!("● {n}s ago")))
    });
    t.wait("the notice gone", |_| t.footer().contains("? help"));
}

#[test]
fn an_opener_that_will_not_run_says_so() {
    let w = World::standard();
    let mut t = Term::start(
        &w,
        Opts {
            env: &[("HACKNEY_OPENER", "/nonexistent/opener")],
            ..Opts::default()
        },
    );
    t.wait_for("Alice opens the thread");
    t.send(b"o");
    t.wait("the failure", |_| {
        t.footer().contains("could not run `/nonexistent/opener`")
    });
    assert!(t.alive());
}

#[test]
fn a_link_goes_to_the_opener_as_it_is_never_through_a_shell() {
    let w = World::new();
    let url = "https://example.com/?q=$(touch pwned1)&x=`touch pwned2`;touch pwned3 'a b' \"c\"";
    w.hn.feed("topstories", &[1]);
    w.hn.item(json!({
        "id": 1, "type": "story", "by": "octocat", "title": "A link full of shell",
        "url": url, "score": 1, "time": now() - 60, "kids": [],
    }));
    let t = Term::start(&w, Opts::default());
    t.wait_story(1);
    t.send(b"o");
    assert_eq!(w.wait_opened(1), [url]);
    for f in ["pwned1", "pwned2", "pwned3"] {
        assert!(!w.dir.path().join(f).exists(), "a shell ran `touch {f}`");
    }
}

#[test]
fn a_search_with_special_characters_reaches_algolia_intact() {
    let w = World::standard();
    w.hn.search(json!([]));
    let t = started(&w);
    let query = "c++ & rust?#1 100% 日本";
    t.send(format!("/{query}\r").as_bytes());
    t.wait("the search, with nothing found", |s| {
        t.nav().contains(&format!("“{query}”")) && s.contains("Nothing here.")
    });
    let searches: Vec<String> =
        w.hn.requests()
            .into_iter()
            .filter(|r| r.starts_with("/algolia/v1/search?"))
            .collect();
    assert_eq!(searches.len(), 1, "{searches:?}");
    assert_eq!(
        query_param(&searches[0], "query").as_deref(),
        Some(query),
        "{searches:?}"
    );
    assert_eq!(
        query_param(&searches[0], "tags").as_deref(),
        Some("story"),
        "{searches:?}"
    );
}

#[test]
fn r_while_the_feed_hangs_asks_again() {
    let w = World::standard();
    w.hn.delay("/v0/topstories.json", Duration::from_secs(60));
    let t = Term::start(&w, Opts::default());
    t.wait("the list loading", |_| t.row(3).contains("loading…"));
    eventually("the first request", || {
        w.hn.hits("/v0/topstories.json") == 1
    });
    w.hn.feed("topstories", &(1..=40).collect::<Vec<_>>());
    t.send(b"r");
    t.wait_for("Alice opens the thread");
    assert_eq!(w.hn.hits("/v0/topstories.json"), 2);
}

#[test]
fn a_refreshed_thread_keeps_its_folds() {
    let w = World::standard();
    let t = started(&w);
    let at = t.wait_find("▾ alice", t.detail_x());
    t.click(at);
    t.wait("alice folded", |_| t.find("▸ alice").is_some());

    // The fresh copy has a new comment, ranked first.
    w.hn.story(1, &headline(1), &[107, 101, 104, 105, 106]);
    let mut tree = story_one_tree();
    tree.as_array_mut().unwrap().push(node(
        107,
        "hubot",
        "A comment that arrived later.",
        json!([]),
    ));
    w.hn.tree(1, tree);
    t.send(b"r");
    t.wait_for("A comment that arrived later.");
    let text = t.text();
    let (row, _) = t
        .find("▸ alice")
        .unwrap_or_else(|| panic!("unfolded:\n{text}"));
    assert!(t.row(row).contains("+2"), "{text}");
    assert!(!text.contains("Bob answers alice"), "{text}");
    assert!(t.find("▾ hubot").unwrap().0 < row, "{text}");
}

#[test]
fn wide_characters_keep_the_columns_and_the_link_targets_in_place() {
    let w = World::new();
    w.hn.feed("topstories", &[1, 2]);
    w.hn.story(1, "日本語のタイトル and a wide headline", &[101]);
    w.hn.story(2, &headline(2), &[]);
    w.hn.tree(
        1,
        json!([node(
            101,
            "octocat",
            "全角の文字が先に来る <a href=\"https://example.com/wide\">wide link</a> after.",
            json!([])
        )]),
    );
    let t = Term::start(&w, Opts::default());
    t.wait_for("wide link");
    let text = t.text();
    let age = |r: u16| t.find_in(" 1h ", r..r + 1, 0).map(|(_, c)| c);
    assert!(age(3).is_some() && age(3) == age(4), "{text}");
    let divider = |r: u16| t.find_in("│", r..r + 1, 40).map(|(_, c)| c);
    assert_eq!(divider(3), divider(4), "{text}");

    let (row, col) = t.find_in("wide link", 3..FOOTER, t.detail_x()).unwrap();
    t.click((row, col));
    assert_eq!(w.wait_opened(1), ["https://example.com/wide"]);
    t.click((row, col + 8)); // its last cell
    assert_eq!(w.wait_opened(2)[1], "https://example.com/wide");
    // The cells either side of it are not the link.
    t.click((row, col - 1));
    t.click((row, col + 9));
    t.send(b"j");
    t.wait_story(2);
    assert_eq!(w.opened().len(), 2, "{:?}", w.opened());
}

#[test]
fn a_link_wrapped_over_two_lines_opens_from_either_line() {
    let w = World::new();
    let url = format!(
        "https://example.com/{}end-of-the-link",
        "long-path-segment/".repeat(7)
    );
    w.hn.feed("topstories", &[1]);
    w.hn.story(1, "A thread with a long link", &[101]);
    w.hn.tree(
        1,
        json!([node(
            101,
            "octocat",
            &format!("See <a href=\"{url}\">{url}</a>"),
            json!([])
        )]),
    );
    let t = Term::start(&w, Opts::default());
    t.wait_for("end-of-the-link");
    let x = t.detail_x();
    let head = t.find_in("https://example.com/long", 3..FOOTER, x).unwrap();
    let tail = t.find_in("end-of-the-link", 3..FOOTER, x).unwrap();
    assert!(tail.0 > head.0, "the link didn't wrap:\n{}", t.text());
    t.click(head);
    t.click(tail);
    assert_eq!(w.wait_opened(2), [url.clone(), url]);
}

/// Every overlay at every size down to a single cell, with keys and clicks
/// thrown in: a panic or an overflow anywhere in drawing ends the process.
#[test]
fn tiny_and_odd_sizes_never_crash_whatever_is_open() {
    let w = World::standard();
    w.hn.search(json!([
        {"objectID": "901", "title": "Rust in the terminal", "url": "https://example.com/901",
         "author": "octocat", "points": 77, "num_comments": 0, "created_at_i": now() - 100},
    ]));
    let mut t = started(&w);
    t.send(b"/rust\r");
    t.wait_story(901);
    t.send(b"1");
    t.wait_story(1);

    // Two rows at least: vt100 itself overflows wrapping a line in one.
    let sizes = [
        (2, 1),
        (2, 3),
        (3, 12),
        (4, 20),
        (6, 40),
        (10, 56),
        (12, 80),
        (30, 119),
        (5, 300),
        (60, 20),
    ];
    for (what, open, close) in [
        ("the list", &b""[..], &b""[..]),
        ("the help", b"?", b"?"),
        ("the prompt", b"/half a query", b"\x1b"),
        ("zoom", b"z", b"z"),
        ("a fold, scrolled", b"x  ", b"xg"),
    ] {
        t.send(open);
        for (rows, cols) in sizes {
            let before = t.raw().len();
            t.resize(rows, cols);
            t.wait_until(&format!("a frame at {rows}x{cols} with {what}"), || {
                t.raw().len() > before || t.eof.load(Ordering::SeqCst)
            });
            t.send(b"\x1b[<65;1;1M\x1b[<64;2;2M");
            t.click((0, 0));
            t.click((rows / 2, cols / 2));
            t.send(b"jknp");
            assert!(t.alive(), "hackney died at {rows}x{cols} with {what}");
        }
        t.resize(ROWS, COLS);
        // The clicks may have landed on any tab: wait for the whole tab bar,
        // which only a frame at full width has room for.
        t.wait(&format!("the full layout back, with {what}"), |_| {
            t.find("hackney │").is_some() && t.row(1).contains("Jobs   ⌕ rust")
        });
        t.send(close);
        t.wait(&format!("{what} closed"), |s| {
            !t.footer().contains("esc cancel") && !s.contains("move through stories")
        });
    }
    t.send(b"1g");
    t.wait_story(1);
    assert!(t.alive());
}

#[test]
fn stacked_the_list_and_the_comments_take_clicks_and_the_wheel() {
    let w = World::standard();
    let t = Term::start(
        &w,
        Opts {
            cols: 80,
            ..Opts::default()
        },
    );
    t.wait_for("Alice opens the thread");
    assert!(!t.rail().contains('┬'), "rail: {}", t.rail());
    let rule = "─".repeat(80);
    let top = (3..FOOTER)
        .find(|&r| t.row(r) == rule)
        .expect("a rule over the comments")
        + 1;

    t.wheel((top + 3, 40), true);
    t.wait("the comments scrolled", |_| {
        t.row(top).contains("article   example.com/1")
    });
    t.wheel((5, 40), true);
    t.wait("the list scrolled", |_| t.row(3).starts_with("   4  ▲"));
    t.click((5, 40));
    t.wait_story(6);
}

/// A reply split by the link, part before the probe's deadline and part
/// after: the part after has no `ESC ]` to mark it, and must not be typed.
#[test]
fn a_colour_reply_split_across_the_probe_deadline_is_not_read_as_keys() {
    let w = World::standard();
    let t = Term::start(
        &w,
        Opts {
            probe: Probe::Split,
            ..Opts::default()
        },
    );
    t.wait_for("Alice opens the thread");
    t.send(b"j");
    t.wait_story(2);
    assert!(t.nav().contains("Top stories"), "nav: {}", t.nav());
    assert!(!t.footer().contains("esc cancel"), "footer: {}", t.footer());
}

#[test]
fn clicks_on_two_different_rows_are_not_a_double_click() {
    let w = World::standard();
    let t = started(&w);
    t.send(format!("{}{}", click_seq((4, 30)), click_seq((5, 30))).as_bytes());
    t.wait_story(3);
    // Then a real double click: anything opened before it would come first.
    t.send(format!("{0}{0}", click_seq((6, 30))).as_bytes());
    assert_eq!(w.wait_opened(1), ["https://example.com/4"]);
}
