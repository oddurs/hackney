//! hackney — Hacker News in the terminal.

mod app;
mod event;
mod hn;
mod probe;
mod text;
mod theme;
mod ui;
mod util;

#[cfg(test)]
#[path = "../tests/support/fake_hn.rs"]
mod fake_hn;

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, read};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use app::{App, Msg};
use hn::Feed;

/// One frame of the spinner.
const SPIN: Duration = Duration::from_millis(80);

/// With nothing going on, wake this often to keep the clock in the nav true.
const IDLE: Duration = Duration::from_secs(1);

const HELP: &str = "\
hackney — Hacker News in the terminal

Usage: hackney [--help | --version]

Keys:
  j/k, ↓/↑      move through stories        enter, o   open the article
  1–7, tab      switch feed                 c          open the discussion
  J/K, space    scroll the comments         /          search
  n/p, x        jump between, fold threads  ?          every key
  q             quit

Environment:
  HACKNEY_OPENER  program links are handed to (default: open, or xdg-open)
  NO_COLOR        draw without colour
";

/// What the command line asks for, when it is not the reader itself: the
/// text to print, and whether it is an answer (stdout, success) or a mistake
/// (stderr, usage error).
fn cli(mut args: impl Iterator<Item = String>) -> Option<(String, bool)> {
    let arg = args.next()?;
    Some(match (arg.as_str(), args.next()) {
        ("-h" | "--help", None) => (HELP.into(), true),
        ("-V" | "--version", None) => (format!("hackney {}\n", env!("CARGO_PKG_VERSION")), true),
        _ => (
            format!(
                "hackney: unexpected argument `{arg}`\n\nUsage: hackney [--help | --version]\n"
            ),
            false,
        ),
    })
}

fn main() -> Result<()> {
    if let Some((text, ok)) = cli(std::env::args().skip(1)) {
        if ok {
            print!("{text}");
            return Ok(());
        }
        eprint!("{text}");
        std::process::exit(2);
    }
    let (mut term, (probed, cut)) = setup()?;
    let (mut app, rx) = App::new(theme::Theme::resolve(probed));
    let result = run(&mut term, &mut app, rx, cut);
    restore(&mut term);
    result
}

fn run(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: Receiver<Msg>,
    cut: Option<probe::Cut>,
) -> Result<()> {
    let mut input = event::Input::after_probe(cut);
    let stop = stop_on_signal();
    read_input(app.sender());
    app.set_feed(Feed::Top);

    let started = Instant::now();
    let mut dirty = true;
    // A click queued behind other input waits for the frame it was aimed at.
    let mut deferred: Option<Event> = None;
    loop {
        if dirty {
            app.spinner = (started.elapsed().as_millis() / SPIN.as_millis()) as usize;
            term.draw(|f| ui::draw(f, app))?;
            dirty = false;
            if let Some(ev) = deferred.take() {
                dirty |= input.handle(app, ev);
            }
        }

        // Sleep until input or a reply arrives, or a timer is due. Then take
        // everything already queued before drawing again: a wheel flick sends
        // dozens of events.
        let due = Instant::now() + wait(app, started);
        let mut next = if dirty {
            rx.try_recv().ok()
        } else {
            match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                // `app` holds a sender, so this can't happen.
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        };
        while let Some(msg) = next {
            match msg {
                Msg::Input(Ok(ev)) if dirty && event::is_click(&ev) => {
                    deferred = Some(ev);
                    break;
                }
                Msg::Input(Ok(ev)) => dirty |= input.handle(app, ev),
                Msg::Input(Err(e)) => anyhow::bail!("reading the terminal: {e}"),
                msg => {
                    app.on_msg(msg);
                    dirty = true;
                }
            }
            next = rx.try_recv().ok();
        }

        // Checked here rather than on timeout alone: a stream of mouse
        // movement would otherwise keep the spinner and clock from moving.
        dirty |= app.tick() || Instant::now() >= due;
        if app.quit || stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

/// How long the loop may sleep: until the story under the cursor is due its
/// comments, the spinner's next frame, or the clock's next second.
fn wait(app: &App, started: Instant) -> Duration {
    let mut wait = IDLE;
    if app.busy() {
        let into = started.elapsed().as_millis() % SPIN.as_millis();
        wait = SPIN - Duration::from_millis(into as u64);
    }
    if let Some(at) = app.settle_at() {
        wait = wait.min(at.saturating_duration_since(Instant::now()));
    }
    wait
}

/// Terminal events, read on their own thread and sent down the same channel
/// as network replies, so either wakes the loop at once.
fn read_input(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            let msg = read().map_err(|e| e.to_string());
            let failed = msg.is_err();
            if tx.send(Msg::Input(msg)).is_err() || failed {
                return;
            }
        }
    });
}

/// SIGTERM, SIGHUP or an external SIGINT ends the loop the ordinary way, so the
/// terminal is restored instead of being left in raw mode.
fn stop_on_signal() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&flag));
    }
    flag
}

type Probe = (probe::Probed, Option<probe::Cut>);

fn setup() -> Result<(Terminal<CrosstermBackend<Stdout>>, Probe)> {
    enable_raw_mode()?;
    // In raw mode and before the input thread owns stdin, so the terminal's
    // replies are read here rather than arriving as keystrokes.
    let probed = probe::query(Duration::from_millis(250));
    let mut out = stdout();
    if let Err(e) = execute!(
        out,
        EnterAlternateScreen,
        Clear(ClearType::All),
        EnableMouseCapture
    ) {
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        return Err(e.into());
    }

    // Leave the terminal usable if the UI thread panics. Background workers
    // turn their own panics into errors.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("main") {
            return;
        }
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        hook(info);
    }));

    match Terminal::new(CrosstermBackend::new(out)) {
        Ok(term) => Ok((term, probed)),
        Err(e) => {
            let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            Err(e.into())
        }
    }
}

/// Best effort throughout: after a SIGHUP the terminal may already be gone.
fn restore(term: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = term.show_cursor();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hn::Story;
    use crate::theme::Theme;

    fn args(a: &[&str]) -> Option<(String, bool)> {
        cli(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_start_the_reader() {
        assert_eq!(args(&[]), None);
    }

    #[test]
    fn help_and_version_answer_without_starting_the_reader() {
        for a in ["-h", "--help"] {
            let (text, ok) = args(&[a]).unwrap();
            assert!(ok && text.starts_with("hackney — Hacker News"), "{a}");
        }
        for a in ["-V", "--version"] {
            let (text, ok) = args(&[a]).unwrap();
            assert!(ok, "{a}");
            assert_eq!(text, format!("hackney {}\n", env!("CARGO_PKG_VERSION")));
        }
    }

    #[test]
    fn anything_else_is_a_usage_error_naming_the_argument() {
        for a in [&["--nope"][..], &["top"], &["--help", "extra"]] {
            let (text, ok) = args(a).unwrap();
            assert!(
                !ok && text.contains(&format!("`{}`", a[0])),
                "{a:?}: {text}"
            );
        }
    }

    fn app() -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.stories = (1..=3)
            .map(|id| Story {
                id,
                ..Story::default()
            })
            .collect();
        a
    }

    fn millis(d: Duration) -> u128 {
        d.as_millis()
    }

    #[test]
    fn with_nothing_going_on_the_loop_sleeps_a_second_to_keep_the_clock_true() {
        let a = app();
        let started = Instant::now() - Duration::from_millis(1234);
        assert_eq!(wait(&a, started), IDLE);
    }

    #[test]
    fn while_busy_the_loop_wakes_for_the_next_spinner_frame() {
        let busy: [fn(&mut App); 3] = [
            |a| a.loading_feed = true,
            |a| a.loading_more = true,
            // The comments on screen are on their way.
            |a| {
                a.pending.insert(1);
            },
        ];
        for (n, make_busy) in busy.into_iter().enumerate() {
            let mut a = app();
            make_busy(&mut a);
            let started = Instant::now() - Duration::from_millis(1234);
            let before = millis(started.elapsed());
            let w = wait(&a, started);
            let after = millis(started.elapsed());
            assert!(w > Duration::ZERO && w <= SPIN, "{n}: {w:?}");
            let spin = millis(SPIN);
            // It ends on a frame boundary as measured at some point during the call.
            assert!(
                (before..=after).any(|e| spin - e % spin == millis(w)),
                "{n}: {w:?} from {before}..={after}ms"
            );
        }
    }

    #[test]
    fn a_story_being_fetched_that_is_not_on_screen_is_no_reason_to_spin() {
        let mut a = app();
        a.pending.insert(2);
        assert_eq!(wait(&a, Instant::now()), IDLE);
    }

    #[test]
    fn while_the_cursor_settles_the_loop_wakes_when_its_comments_are_due() {
        let mut a = app();
        a.select(1);
        let at = a.settle_at().expect("settling");
        let before = Instant::now();
        let w = wait(&a, Instant::now());
        let after = Instant::now();
        assert!(w <= at.saturating_duration_since(before), "{w:?}");
        assert!(w >= at.saturating_duration_since(after), "{w:?}");
        assert!(w < IDLE);
    }

    #[test]
    fn busy_and_settling_the_loop_wakes_for_the_spinner_if_that_comes_first() {
        let mut a = app();
        a.select(1);
        a.loading_feed = true;
        let at = a.settle_at().unwrap();
        let before = Instant::now();
        let w = wait(&a, before);
        assert!(w <= SPIN && w <= at.saturating_duration_since(before));
    }
}
