//! A made-up Hacker News for screenshots and demos: the test suite's fake
//! server, filled with a front page and a few busy threads. Nothing in it is
//! real — every story, user and comment is invented.
//!
//! ```sh
//! cargo run --example demo_server
//! # prints the two variables to point hackney at it, then serves until killed
//! ```

#[path = "../tests/support/fake_hn.rs"]
mod fake_hn;

use fake_hn::{FakeHn, node, now};
use serde_json::{Value, json};

/// Title, domain and path (None for a text post), author, score, comments,
/// age in minutes.
type Story = (
    &'static str,
    Option<&'static str>,
    &'static str,
    u64,
    u64,
    i64,
);

#[rustfmt::skip]
const STORIES: &[Story] = &[
    ("Show HN: A two-pane Hacker News reader for the terminal", Some("example.dev/hackney"), "tidewater", 412, 138, 95),
    ("The quiet death of the 80-column terminal", Some("blog.example.org/eighty-columns"), "ferrous", 356, 211, 180),
    ("Ask HN: What's the best technical book you read this year?", None, "quietloop", 298, 486, 240),
    ("SQLite's query planner, explained with diagrams", Some("notes.example.net/sqlite-planner"), "bramble", 267, 74, 130),
    ("Why your Rust build is slow, and six ways to fix it", Some("rustacean.example.com/slow-builds"), "cargocult", 244, 102, 300),
    ("A deep dive into how terminals render text", Some("example.com/terminal-rendering"), "glyphwright", 231, 57, 360),
    ("Launch HN: Tallyho (YC S26) – Spreadsheets that check their own maths", None, "tallyho_dan", 189, 143, 420),
    ("The unreasonable effectiveness of plain text", Some("essays.example.org/plain-text"), "marginalia", 176, 88, 480),
    ("Postgres 19 released", Some("postgresql.example.org/about/news/19"), "pgfan", 522, 190, 520),
    ("I replaced my home lab with a single fanless box", Some("homelab.example.net/one-box"), "sparrowhawk", 158, 131, 600),
    ("A visual guide to consistent hashing", Some("example.com/consistent-hashing"), "ringbuf", 143, 29, 650),
    ("Show HN: I built a tiny Forth that boots from a floppy image", Some("forth.example.dev"), "stackwise", 139, 41, 700),
    ("The hidden cost of retries", Some("sre.example.org/retries"), "backoff", 127, 66, 760),
    ("Ask HN: How do you keep up with papers in your field?", None, "phdlater", 121, 97, 820),
    ("Designing a keyboard-first interface", Some("design.example.com/keyboard-first"), "kerning", 118, 38, 880),
    ("We moved our monorepo to jj and nobody noticed", Some("eng.example.io/jj-migration"), "treeish", 112, 84, 940),
    ("Reading the Unicode standard so you don't have to", Some("example.org/unicode-notes"), "codepoint", 105, 45, 1000),
    ("The case for boring technology, ten years on", Some("blog.example.com/boring-ten-years"), "oldhand", 99, 120, 1080),
    ("Memory-safe languages and the long tail of C", Some("research.example.edu/long-tail"), "bounds", 96, 153, 1150),
    ("How a one-line config change took down search for an hour", Some("status.example.com/postmortem"), "pagerduty_ish", 91, 61, 1210),
    ("Show HN: A browser extension that turns tabs into a reading list", Some("example.app/tabshelf"), "tabhoarder", 87, 33, 1300),
    ("Why we still write our own parsers", Some("compilers.example.dev/own-parsers"), "lexer", 83, 47, 1360),
    ("Tallyho (YC S26) is hiring a founding engineer", None, "tallyho_dan", 0, 0, 1400),
    ("The physics of a perfect espresso shot", Some("science.example.org/espresso"), "crema", 79, 70, 1450),
    ("Understanding io_uring from first principles", Some("kernel.example.net/io-uring"), "ringzero", 76, 22, 1500),
    ("What I learned maintaining an open source project for a decade", Some("example.com/decade"), "maintainer", 74, 58, 1600),
    ("A love letter to man pages", Some("blog.example.net/man-pages"), "rtfm", 71, 36, 1700),
    ("Show HN: Offline-first notes with plain Markdown files", Some("notes.example.app"), "inkwell", 68, 44, 1800),
    ("The surprising history of the tab character", Some("example.org/tab-history"), "whitespace", 64, 27, 1900),
    ("Ask HN: Is anyone still using Emacs for everything?", None, "chordal", 60, 212, 2000),
];

fn main() {
    let hn = FakeHn::start();
    let ids: Vec<u64> = (1..=STORIES.len() as u64).collect();

    for (i, &(title, url, by, score, comments, age)) in STORIES.iter().enumerate() {
        let id = ids[i];
        let kids: Vec<u64> = if comments > 0 {
            vec![id * 1000 + 1, id * 1000 + 2]
        } else {
            vec![]
        };
        let mut item = json!({
            "id": id, "type": "story", "by": by, "title": title, "score": score,
            "time": now() - age * 60, "descendants": comments, "kids": kids,
        });
        match url {
            Some(u) => item["url"] = json!(format!("https://{u}")),
            None => item["text"] = json!(ask_text(title)),
        }
        hn.item(item);
        if comments > 0 {
            hn.tree(id, small_thread(id));
        }
    }
    hn.tree(1, busy_thread());
    hn.item({
        let mut s = hn_story(1);
        s["kids"] = json!([1001, 1002, 1003, 1004]);
        s
    });
    hn.tree(3, ask_thread());
    hn.item({
        let mut s = hn_story(3);
        s["kids"] = json!([3001, 3002, 3003]);
        s
    });

    hn.feed("topstories", &ids);
    hn.feed("newstories", &ids.iter().rev().copied().collect::<Vec<_>>());
    hn.feed("beststories", &[9, 1, 2, 3, 4, 5, 6, 8, 10, 19]);
    hn.feed("askstories", &[3, 14, 30]);
    hn.feed("showstories", &[1, 12, 21, 28]);
    hn.feed("jobstories", &[23]);
    hn.search(json!(
        [
            (1, "terminal"),
            (6, "terminal"),
            (2, "terminal"),
            (27, "man pages")
        ]
        .iter()
        .map(|&(id, _)| {
            let (title, url, by, score, comments, age) = STORIES[id - 1];
            json!({
                "objectID": id.to_string(), "title": title, "author": by,
                "url": url.map(|u| format!("https://{u}")), "points": score,
                "num_comments": comments, "created_at_i": now() - age * 60,
            })
        })
        .collect::<Vec<_>>()
    ));

    println!("export HACKNEY_FIREBASE_URL={}", hn.firebase());
    println!("export HACKNEY_ALGOLIA_URL={}", hn.algolia());
    // Serve until killed.
    loop {
        std::thread::park();
    }
}

fn hn_story(id: u64) -> Value {
    let (title, url, by, score, comments, age) = STORIES[id as usize - 1];
    let mut s = json!({
        "id": id, "type": "story", "by": by, "title": title, "score": score,
        "time": now() - age * 60, "descendants": comments,
    });
    match url {
        Some(u) => s["url"] = json!(format!("https://{u}")),
        None => s["text"] = json!(ask_text(title)),
    }
    s
}

fn ask_text(title: &str) -> String {
    if title.starts_with("Ask HN: What's the best technical book") {
        "Mine was a slim book on database internals that finally made B-trees click. \
         I'm looking for something similar: dense, practical, and written by someone who \
         has clearly shipped the thing they're describing.<p>Bonus points for books \
         that changed how you work day to day, not just what you know."
            .into()
    } else if title.contains("hiring") {
        "We're a small team building spreadsheets that verify their own formulas. \
         Rust, TypeScript, and a lot of property tests. Remote-friendly, EU and US time zones."
            .into()
    } else {
        "Curious how others handle this. What has actually worked for you, and what did \
         you try and give up on?"
            .into()
    }
}

/// The thread on the first story: long enough to scroll, nested, with code,
/// a quote, a link, and the poster answering.
fn busy_thread() -> Value {
    json!([
        node(
            1001,
            "ferrous",
            "This is lovely. The two-pane layout is exactly how I read HN in the browser anyway: \
             skim the list, open the one thread worth reading.<p>How does it decide when to fetch \
             the comments? Scrolling fast through the list doesn't seem to fire a request per row.",
            json!([
                node(
                    1101,
                    "tidewater",
                    "It waits until the cursor rests on a story for a moment, then fetches that thread \
                     and the next few in the background, so moving on is usually instant.",
                    json!([node(
                        1201,
                        "ferrous",
                        "Clever. That's the part that makes it feel fast.",
                        json!([])
                    ),])
                ),
                node(
                    1102,
                    "ringbuf",
                    "&gt; Scrolling fast through the list doesn't seem to fire a request per row.<p>\
                     Debouncing plus prefetch is the right call. I'd love to see the same trick in more TUIs.",
                    json!([])
                ),
            ])
        ),
        node(
            1002,
            "glyphwright",
            "Nice touch that it inherits the terminal's own colours instead of shipping a theme. \
             It looks native in every colour scheme I tried.",
            json!([node(
                1103,
                "kerning",
                "Agreed. Everything is drawn from the palette, and the selection band is mixed \
                     from your actual foreground and background.",
                json!([])
            ),])
        ),
        node(
            1003,
            "stackwise",
            "Tried it over SSH on a slow link and it held up fine. For anyone curious, installing \
             from source is one line:<p><pre><code>  cargo install hackney\n</code></pre>",
            json!([])
        ),
        node(
            1004,
            "marginalia",
            "Feature request: a way to fold every top-level thread at once. \
             See <a href=\"https://example.com/folding-ideas\" rel=\"nofollow\">https://example.com/folding-ideas</a> \
             for how a few other readers do it.",
            json!([node(
                1104,
                "tidewater",
                "Noted! For now <i>x</i> folds the thread you're reading and <i>n</i>/<i>p</i> jump between them.",
                json!([])
            ),])
        ),
    ])
}

fn ask_thread() -> Value {
    json!([
        node(
            3001,
            "bramble",
            "A book on designing data-intensive systems. Ten years on and I still reach for it \
             whenever a design review turns to replication.",
            json!([node(
                3101,
                "pgfan",
                "Seconded. The chapter on transactions alone is worth it.",
                json!([])
            ),])
        ),
        node(
            3002,
            "lexer",
            "A compilers book that builds two interpreters from scratch. It's the first one that \
             made me feel I could write a language, not just read about them.",
            json!([])
        ),
        node(
            3003,
            "oldhand",
            "The classic on the Unix programming environment. Old, short, and it explains why the \
             tools are shaped the way they are.",
            json!([])
        ),
    ])
}

/// Two short threads for every other story, so any row opens to something.
fn small_thread(id: u64) -> Value {
    json!([
        node(
            id * 1000 + 1,
            "quietloop",
            "Good write-up. The part about the trade-offs matched my experience almost exactly.",
            json!([node(
                id * 1000 + 11,
                "backoff",
                "Same here, though we hit the limits sooner than the post suggests.",
                json!([])
            ),])
        ),
        node(
            id * 1000 + 2,
            "sparrowhawk",
            "I'd be curious to see numbers from a larger deployment before drawing conclusions.",
            json!([])
        ),
    ])
}
