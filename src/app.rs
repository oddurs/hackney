//! Application state: what is loaded, what is selected, and what keys and
//! clicks do to it. Network work runs on background threads and reports back
//! over a channel, as in rigor. Input arrives over the same channel, so a
//! reply is drawn the moment it lands rather than at the next timer tick.

use ratatui::crossterm::event::Event;
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crate::hn::{Client, Comment, Feed, Story};
use crate::text::{self, Body};
use crate::theme::Theme;
use crate::ui;
use crate::util::now_secs;

/// Stories fetched per page of a feed.
pub const PAGE: usize = 30;

/// How long the cursor has to rest on a story before its comments are
/// fetched, so running down the list with `j` doesn't fire a request per row.
const SETTLE: Duration = Duration::from_millis(150);

/// Stories after the one being read whose comments are fetched ahead of
/// time (and one before), so moving on shows them at once.
const AHEAD: usize = 3;

/// Thread fetches in flight before prefetching holds off.
const MAX_PENDING: usize = 6;

/// Rows from the end of the list at which the next page is requested.
const MORE_AT: usize = 10;

/// Threads kept in memory; the least recently fetched go first.
const KEEP_THREADS: usize = 64;

/// A thread older than this is fetched again when it is next read, and shown
/// as it was in the meantime.
const THREAD_TTL: Duration = Duration::from_secs(300);

/// A cached feed older than this, in seconds, is fetched again when its tab
/// is opened, behind the copy already on screen.
const FEED_TTL: i64 = 120;

/// Stack for workers. HN comment trees nest deep, and parsing and dropping
/// one recurses once per level; the default 2 MiB is not a safe bound.
const WORKER_STACK: usize = 64 << 20;

pub enum Msg {
    /// A terminal event, or the reason there will be no more.
    Input(Result<Event, String>),
    Feed {
        generation: u64,
        result: Result<Listing, String>,
    },
    More {
        generation: u64,
        /// Where the page starts in `ids`, to ask again if it fails.
        from: usize,
        result: Result<Vec<Story>, String>,
    },
    Thread {
        id: u64,
        result: Result<Thread, String>,
    },
}

/// A feed as fetched: every id, and the stories for the first `requested`.
pub struct Listing {
    pub ids: Vec<u64>,
    pub requested: usize,
    pub stories: Vec<Story>,
}

/// `open` or `xdg-open`, unless `HACKNEY_OPENER` names another program (a
/// test's recorder, say). Under unit tests, `true`: nothing opens.
fn opener() -> String {
    if cfg!(test) {
        return "true".into();
    }
    std::env::var("HACKNEY_OPENER").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        }
        .into()
    })
}

/// Run `work` off the UI thread and always answer: a panic inside becomes an
/// error rather than a reply that never comes.
fn spawn_reply<T: Send + 'static>(
    tx: Sender<Msg>,
    wrap: impl FnOnce(Result<T, String>) -> Msg + Send + 'static,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) {
    std::thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "internal error: a background task panicked"
                    ))
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(wrap(res));
        })
        .expect("spawn a worker thread");
}

/// One comment, flattened out of the tree in reading order.
pub struct Node {
    pub id: u64,
    pub by: String,
    pub time: i64,
    pub body: Body,
    pub depth: usize,
    /// Index one past this comment's last descendant.
    pub end: usize,
    pub folded: bool,
}

impl Node {
    pub fn replies(&self, index: usize) -> usize {
        self.end - index - 1
    }
}

pub struct Thread {
    pub story: Story,
    pub text: Option<Body>,
    pub nodes: Vec<Node>,
    pub fetched: Instant,
    /// Changes whenever what the thread draws changes — a fold, or a fresh
    /// copy — so the rendered document can be kept until it does.
    pub rev: u64,
}

impl Thread {
    pub fn new(story: Story, comments: Vec<Comment>) -> Self {
        fn walk(nodes: &mut Vec<Node>, comments: Vec<Comment>) {
            for c in comments {
                let i = nodes.len();
                nodes.push(Node {
                    id: c.id,
                    by: c.by,
                    time: c.time,
                    body: text::parse(&c.text),
                    depth: c.depth,
                    end: 0,
                    folded: false,
                });
                walk(nodes, c.children);
                nodes[i].end = nodes.len();
            }
        }
        let mut nodes = Vec::new();
        walk(&mut nodes, comments);
        Thread {
            text: story.text.as_deref().map(text::parse),
            story,
            nodes,
            fetched: Instant::now(),
            rev: 0,
        }
    }

    /// Indices of the comments on screen: a folded comment shows, its
    /// replies don't.
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.nodes.len() {
            out.push(i);
            i = if self.nodes[i].folded {
                self.nodes[i].end
            } else {
                i + 1
            };
        }
        out
    }
}

/// What a click on a recorded region does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Tab(Feed),
    Row(usize),
    Fold(usize),
    Open(String),
}

/// Screen regions recorded during the last draw so clicks can be resolved.
#[derive(Default)]
pub struct Hits {
    pub zones: Vec<(Rect, Target)>,
    pub list: Rect,
    pub detail: Rect,
}

/// Where things landed in the detail pane on the last draw, for the keys
/// that jump between threads.
#[derive(Default)]
pub struct Layout {
    /// (line, node) for every visible top-level comment.
    pub roots: Vec<(usize, usize)>,
}

/// A feed that is not on screen, kept so its tab opens at once.
struct Stash {
    ids: Vec<u64>,
    requested: usize,
    stories: Vec<Story>,
    selected: usize,
    offset: usize,
    fetched: i64,
}

pub struct App {
    pub theme: Theme,
    pub feed: Feed,
    /// The last search; the Search tab exists once there is one.
    pub query: String,
    pub ids: Vec<u64>,
    /// How many of `ids` have been requested so far.
    pub requested: usize,
    pub stories: Vec<Story>,
    pub selected: usize,
    pub offset: usize,
    /// Whether the list scrolls to keep the selection in view. The wheel
    /// turns it off, so the list can be browsed without moving the cursor.
    follow: bool,
    pub detail_scroll: usize,
    /// Bumped on every feed switch, so a late reply for the old feed is dropped.
    pub generation: u64,
    pub loading_feed: bool,
    pub loading_more: bool,
    /// Where the page on its way starts, to ask for it again if its reply
    /// is dropped by a switch of feed.
    more_from: usize,
    /// The last page failed. Only the user moving asks for it again; the
    /// automatic asks (a pane with room, every frame) wait, or a failing
    /// page would be requested on every frame.
    more_failed: bool,
    feeds: HashMap<Feed, Stash>,
    pub threads: HashMap<u64, Thread>,
    pub pending: HashSet<u64>,
    /// Why a story's comments could not be fetched, until they can.
    pub thread_errors: HashMap<u64, String>,
    /// Threads fetched before this are out of date (set by refresh).
    stale_before: Instant,
    rev: u64,
    /// A story the cursor landed on, and when its comments are due.
    settling: Option<(u64, Instant)>,
    pub read: HashSet<u64>,
    pub zoom: bool,
    /// The search being typed, while the prompt is open.
    pub prompt: Option<String>,
    /// A failure loading the feed itself.
    pub error: Option<String>,
    pub notice: Option<(String, i64)>,
    pub last_fetch: i64,
    pub show_help: bool,
    pub quit: bool,
    pub spinner: usize,
    pub hits: Hits,
    pub layout: Layout,
    pub doc: ui::Cache,
    /// The program links are handed to.
    pub opener: String,
    client: Client,
    tx: Sender<Msg>,
}

impl App {
    pub fn new(theme: Theme) -> (Self, Receiver<Msg>) {
        // Unit tests never reach the network or a browser by accident: a test
        // that wants either says so with `with_client` or `opener`.
        #[cfg(test)]
        let client = Client::at(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            Duration::from_secs(1),
        );
        #[cfg(not(test))]
        let client = Client::new();
        Self::with_client(theme, client)
    }

    pub fn with_client(theme: Theme, client: Client) -> (Self, Receiver<Msg>) {
        let (tx, rx) = channel();
        let app = Self {
            theme,
            feed: Feed::Top,
            query: String::new(),
            ids: Vec::new(),
            requested: 0,
            stories: Vec::new(),
            selected: 0,
            offset: 0,
            follow: true,
            detail_scroll: 0,
            generation: 0,
            loading_feed: false,
            loading_more: false,
            more_from: 0,
            more_failed: false,
            feeds: HashMap::new(),
            threads: HashMap::new(),
            pending: HashSet::new(),
            thread_errors: HashMap::new(),
            stale_before: Instant::now(),
            rev: 0,
            settling: None,
            read: HashSet::new(),
            zoom: false,
            prompt: None,
            error: None,
            notice: None,
            last_fetch: 0,
            show_help: false,
            quit: false,
            spinner: 0,
            hits: Hits::default(),
            layout: Layout::default(),
            doc: ui::Cache::default(),
            opener: opener(),
            client,
            tx,
        };
        (app, rx)
    }

    /// For the thread that feeds terminal input into the loop.
    pub fn sender(&self) -> Sender<Msg> {
        self.tx.clone()
    }

    // ---------------------------------------------------------------- loading

    pub fn tabs(&self) -> Vec<Feed> {
        let mut tabs = Feed::TABS.to_vec();
        if !self.query.is_empty() {
            tabs.push(Feed::Search);
        }
        tabs
    }

    /// Switch to `feed`: at once from the cache if it was open before (fetched
    /// again behind it if old), otherwise from the network.
    pub fn set_feed(&mut self, feed: Feed) {
        if feed == self.feed && (self.loading_feed || !self.ids.is_empty()) {
            return;
        }
        self.drop_page_in_flight();
        let old = std::mem::replace(&mut self.feed, feed);
        if old != feed && !self.ids.is_empty() {
            let stash = Stash {
                ids: std::mem::take(&mut self.ids),
                requested: self.requested,
                stories: std::mem::take(&mut self.stories),
                selected: self.selected,
                offset: self.offset,
                fetched: self.last_fetch,
            };
            self.feeds.insert(old, stash);
        }
        self.generation += 1;
        self.settling = None;
        self.loading_feed = false;
        self.loading_more = false;
        self.error = None;
        self.detail_scroll = 0;
        match self.feeds.remove(&feed) {
            Some(s) => {
                let stale = now_secs() - s.fetched > FEED_TTL;
                self.ids = s.ids;
                self.requested = s.requested;
                self.stories = s.stories;
                self.offset = s.offset;
                self.last_fetch = s.fetched;
                self.selected = s.selected.min(self.stories.len().saturating_sub(1));
                self.select(self.selected);
                if stale {
                    self.load(true);
                }
            }
            None => self.load(false),
        }
    }

    /// Fetch the current feed. With `keep`, what is on screen stays until the
    /// new list arrives, as many stories are fetched as were loaded, and the
    /// cursor stays on the same story.
    fn load(&mut self, keep: bool) {
        self.drop_page_in_flight();
        self.generation += 1;
        self.loading_feed = true;
        if !keep {
            self.ids.clear();
            self.stories.clear();
            self.requested = 0;
            self.selected = 0;
            self.offset = 0;
            self.follow = true;
            self.detail_scroll = 0;
            self.settling = None;
        }
        let count = self.requested.max(PAGE);
        let (client, generation, query, feed) = (
            self.client.clone(),
            self.generation,
            self.query.clone(),
            self.feed,
        );
        spawn_reply(
            self.tx.clone(),
            move |result| Msg::Feed { generation, result },
            move || {
                if feed == Feed::Search {
                    let stories = client.search(&query)?;
                    let ids: Vec<u64> = stories.iter().map(|s| s.id).collect();
                    Ok(Listing {
                        requested: ids.len(),
                        ids,
                        stories,
                    })
                } else {
                    let ids = client.feed(feed)?;
                    let requested = ids.len().min(count);
                    let stories = client.stories(&ids[..requested])?;
                    Ok(Listing {
                        ids,
                        requested,
                        stories,
                    })
                }
            },
        );
    }

    /// The next page, once the cursor or the view gets near the end of what
    /// is loaded.
    /// A page on its way is about to have its reply dropped (the generation
    /// moves on): count it as never asked for, or it is skipped for good.
    fn drop_page_in_flight(&mut self) {
        self.more_failed = false;
        if self.loading_more {
            self.requested = self.more_from;
            self.loading_more = false;
        }
    }

    pub fn load_more(&mut self) {
        if self.loading_feed || self.loading_more || self.requested >= self.ids.len() {
            return;
        }
        self.loading_more = true;
        let from = self.requested;
        let end = (from + PAGE).min(self.ids.len());
        let ids = self.ids[from..end].to_vec();
        self.requested = end;
        self.more_from = from;
        self.more_failed = false;
        let (client, generation) = (self.client.clone(), self.generation);
        spawn_reply(
            self.tx.clone(),
            move |result| Msg::More {
                generation,
                from,
                result,
            },
            move || client.stories(&ids),
        );
    }

    fn fresh(&self, t: &Thread) -> bool {
        t.fetched >= self.stale_before && t.fetched.elapsed() < THREAD_TTL
    }

    /// Fetch a story's comments unless a fresh copy is here or on its way. An
    /// old copy stays on screen until the new one replaces it.
    fn fetch_thread(&mut self, id: u64) {
        if self.pending.contains(&id) || self.threads.get(&id).is_some_and(|t| self.fresh(t)) {
            return;
        }
        self.pending.insert(id);
        let client = self.client.clone();
        spawn_reply(
            self.tx.clone(),
            move |result| Msg::Thread { id, result },
            move || {
                let (story, comments) = client.thread(id)?;
                Ok(Thread::new(story, comments))
            },
        );
    }

    /// Comments for the stories around row `i` that have never been fetched.
    fn prefetch(&mut self, i: usize) {
        for j in (i + 1..=i + AHEAD).chain(i.checked_sub(1)) {
            if self.pending.len() >= MAX_PENDING {
                return;
            }
            if let Some(id) = self.stories.get(j).map(|s| s.id)
                && !self.threads.contains_key(&id)
                && !self.thread_errors.contains_key(&id)
            {
                self.fetch_thread(id);
            }
        }
    }

    /// Everything fetched again, with what is on screen kept until the new
    /// copies arrive.
    pub fn refresh(&mut self) {
        self.stale_before = Instant::now();
        self.thread_errors.clear();
        for s in self.feeds.values_mut() {
            s.fetched = 0;
        }
        if let Some(id) = self.selected_story().map(|s| s.id) {
            self.fetch_thread(id);
        }
        self.load(!self.stories.is_empty());
    }

    pub fn search(&mut self, query: String) {
        self.query = query;
        self.feeds.remove(&Feed::Search);
        if self.feed == Feed::Search {
            self.load(false);
        } else {
            self.set_feed(Feed::Search);
        }
    }

    /// Called every turn of the event loop. Starts whatever is due; never
    /// waits. Returns whether it started anything.
    pub fn tick(&mut self) -> bool {
        let Some((id, at)) = self.settling else {
            return false;
        };
        if Instant::now() < at {
            return false;
        }
        self.settling = None;
        self.read.insert(id);
        self.fetch_thread(id);
        self.prefetch(self.selected);
        true
    }

    /// Something the user is waiting on: the list, or the comments on screen.
    pub fn busy(&self) -> bool {
        self.loading_feed
            || self.loading_more
            || self
                .selected_story()
                .is_some_and(|s| self.pending.contains(&s.id))
    }

    /// When the story under the cursor is due to have its comments fetched.
    pub fn settle_at(&self) -> Option<Instant> {
        self.settling.map(|(_, at)| at)
    }

    pub fn settling(&self) -> bool {
        self.settling.is_some()
    }

    fn bump(&mut self) -> u64 {
        self.rev += 1;
        self.rev
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Input(_) => {}
            Msg::Feed { generation, result } if generation == self.generation => {
                self.loading_feed = false;
                match result {
                    Ok(l) => {
                        let keep = self.selected_story().map(|s| s.id);
                        self.ids = l.ids;
                        self.requested = l.requested;
                        self.stories = l.stories;
                        self.last_fetch = now_secs();
                        self.error = None;
                        match keep.and_then(|id| self.stories.iter().position(|s| s.id == id)) {
                            Some(i) => self.selected = i,
                            None => {
                                self.selected = 0;
                                self.detail_scroll = 0;
                            }
                        }
                        self.select(self.selected);
                        // Nothing to debounce: fetch the comments now.
                        self.settling = self.settling.map(|(id, _)| (id, Instant::now()));
                        self.fill();
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Msg::More {
                generation,
                from,
                result,
            } if generation == self.generation => {
                self.loading_more = false;
                match result {
                    Ok(stories) => {
                        let first = self.stories.is_empty();
                        self.stories.extend(stories);
                        self.error = None;
                        // The first live stories came on a later page (the
                        // first was all dead): something has to settle.
                        if first {
                            self.select(self.selected);
                        }
                        self.fill();
                    }
                    Err(e) => {
                        // Ask for the same page next time rather than skip it.
                        self.requested = from;
                        self.more_failed = true;
                        self.error = Some(e);
                    }
                }
            }
            Msg::Feed { .. } | Msg::More { .. } => {}
            Msg::Thread { id, result } => {
                self.pending.remove(&id);
                match result {
                    Ok(mut thread) => {
                        self.thread_errors.remove(&id);
                        // A fresh copy keeps the folds of the one it replaces.
                        if let Some(old) = self.threads.get(&id) {
                            let folded: HashSet<u64> = old
                                .nodes
                                .iter()
                                .filter(|n| n.folded)
                                .map(|n| n.id)
                                .collect();
                            for n in &mut thread.nodes {
                                n.folded = folded.contains(&n.id);
                            }
                        }
                        thread.rev = self.bump();
                        self.threads.insert(id, thread);
                        self.evict();
                    }
                    // An older copy on screen is still worth reading.
                    Err(e) if !self.threads.contains_key(&id) => {
                        self.thread_errors.insert(id, e);
                    }
                    Err(_) => {}
                }
            }
        }
    }

    fn evict(&mut self) {
        if self.threads.len() <= KEEP_THREADS {
            return;
        }
        let keep = self.selected_story().map(|s| s.id);
        let oldest = self
            .threads
            .iter()
            .filter(|(id, _)| Some(**id) != keep)
            .min_by_key(|(_, t)| t.fetched)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.threads.remove(&id);
        }
    }

    // ------------------------------------------------------------ selection

    pub fn selected_story(&self) -> Option<&Story> {
        self.stories.get(self.selected)
    }

    pub fn thread(&self) -> Option<&Thread> {
        self.threads.get(&self.selected_story()?.id)
    }

    pub fn select(&mut self, i: usize) {
        let Some(id) = self.stories.get(i).map(|s| s.id) else {
            return;
        };
        if self.selected_story().map(|s| s.id) != Some(id) {
            self.detail_scroll = 0;
        }
        self.selected = i;
        self.follow = true;
        self.settling = Some((id, Instant::now() + SETTLE));
        if i + MORE_AT >= self.stories.len() {
            self.load_more();
        }
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.stories.is_empty() {
            return;
        }
        let n = self.stories.len() as isize;
        let i = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        if i != self.selected {
            self.select(i);
        }
    }

    pub fn cycle_feed(&mut self, delta: isize) {
        let tabs = self.tabs();
        let cur = tabs.iter().position(|f| *f == self.feed).unwrap_or(0) as isize;
        let n = tabs.len() as isize;
        self.set_feed(tabs[(cur + delta).rem_euclid(n) as usize]);
    }

    /// The wheel over the list moves the view, not the cursor: browsing
    /// shouldn't swap the comments out from under you on every notch.
    pub fn scroll_list(&mut self, delta: isize) {
        let height = self.hits.list.height as usize;
        let max = self.stories.len().saturating_sub(height);
        self.offset = self.offset.saturating_add_signed(delta).min(max);
        self.follow = false;
        if self.offset + height + MORE_AT >= self.stories.len() {
            self.load_more();
        }
    }

    /// The next page, if the list pane has room it can't fill.
    fn fill(&mut self) {
        let height = self.hits.list.height as usize;
        self.want_more(height);
    }

    /// The next page when the view is near the end, asked for without the
    /// user doing anything — so not again after a failure (see `more_failed`).
    fn want_more(&mut self, height: usize) {
        if !self.more_failed && self.offset + height + MORE_AT >= self.stories.len() {
            self.load_more();
        }
    }

    /// Keep the view inside the list, and the selection inside the view
    /// unless the wheel has moved it away.
    pub fn clamp_scroll(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.follow {
            if self.selected < self.offset {
                self.offset = self.selected;
            } else if self.selected >= self.offset + height {
                self.offset = self.selected + 1 - height;
            }
        }
        self.offset = self.offset.min(self.stories.len().saturating_sub(height));
        // A pane grown past the end of the list (a resize) asks for more.
        self.want_more(height);
    }

    // ------------------------------------------------------------- comments

    pub fn scroll_detail(&mut self, delta: isize) {
        self.detail_scroll = self.detail_scroll.saturating_add_signed(delta);
    }

    pub fn toggle_fold(&mut self, node: usize) {
        let Some(id) = self.selected_story().map(|s| s.id) else {
            return;
        };
        let rev = self.bump();
        if let Some(t) = self.threads.get_mut(&id)
            && let Some(n) = t.nodes.get_mut(node)
        {
            n.folded = !n.folded;
            t.rev = rev;
        }
    }

    /// `n` / `p`: the next or previous top-level comment to the top of the pane.
    pub fn jump(&mut self, forward: bool) {
        let cur = self.detail_scroll;
        let roots = &self.layout.roots;
        let target = if forward {
            roots.iter().find(|(line, _)| *line > cur)
        } else {
            roots.iter().rev().find(|(line, _)| *line < cur)
        };
        match target {
            Some((line, _)) => self.detail_scroll = *line,
            None if !forward => self.detail_scroll = 0,
            None => {}
        }
    }

    /// `x`: fold or unfold the top-level thread being read, keeping it in place.
    pub fn fold_current(&mut self) {
        let cur = self.detail_scroll;
        let roots = &self.layout.roots;
        let Some(&(line, node)) = roots
            .iter()
            .rev()
            .find(|(line, _)| *line <= cur)
            .or(roots.first())
        else {
            return;
        };
        self.toggle_fold(node);
        self.detail_scroll = line;
    }

    // --------------------------------------------------------------- actions

    pub fn note(&mut self, s: impl Into<String>) {
        self.notice = Some((s.into(), now_secs()));
    }

    pub fn open_url(&mut self, url: &str) {
        // Web links only: `open` would as happily launch a `file://` app, and
        // an argument starting with `-` would be taken for an option.
        let scheme = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        if !scheme.starts_with("https://") && !scheme.starts_with("http://") {
            self.note(format!("not opening {url}: not a web link"));
            return;
        }
        let bin = &self.opener;
        match Command::new(bin)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reap it, or every link opened leaves a zombie behind.
                std::thread::spawn(move || child.wait());
                self.note(format!("opened {url}"));
            }
            Err(e) => self.note(format!("could not run `{bin}`: {e}")),
        }
    }

    pub fn open_article(&mut self) {
        if let Some(url) = self.selected_story().map(Story::link) {
            self.open_url(&url);
        }
    }

    pub fn open_discussion(&mut self) {
        if let Some(url) = self.selected_story().map(Story::hn_url) {
            self.open_url(&url);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub fn comment(by: &str, text: &str, depth: usize, children: Vec<Comment>) -> Comment {
        static ID: AtomicU64 = AtomicU64::new(1000);
        Comment {
            id: ID.fetch_add(1, Ordering::Relaxed),
            by: by.into(),
            time: now_secs() - 600,
            text: text.into(),
            depth,
            children,
        }
    }

    fn thread() -> Thread {
        Thread::new(
            Story::default(),
            vec![
                comment(
                    "a",
                    "one",
                    0,
                    vec![
                        comment("b", "two", 1, vec![comment("c", "three", 2, vec![])]),
                        comment("d", "four", 1, vec![]),
                    ],
                ),
                comment("e", "five", 0, vec![]),
            ],
        )
    }

    fn story(id: u64) -> Story {
        Story {
            id,
            title: format!("story {id}"),
            ..Story::default()
        }
    }

    /// An app showing stories `ids` of the Top feed, as if just fetched.
    fn loaded(ids: &[u64]) -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.loading_feed = true;
        a.on_msg(Msg::Feed {
            generation: a.generation,
            result: Ok(Listing {
                ids: ids.to_vec(),
                requested: ids.len(),
                stories: ids.iter().map(|&id| story(id)).collect(),
            }),
        });
        a
    }

    #[test]
    fn flattening_records_each_subtree() {
        let t = thread();
        let who: Vec<&str> = t.nodes.iter().map(|n| n.by.as_str()).collect();
        assert_eq!(who, ["a", "b", "c", "d", "e"]);
        assert_eq!(t.nodes[0].replies(0), 3);
        assert_eq!(t.nodes[1].replies(1), 1);
        assert_eq!(t.nodes[4].replies(4), 0);
    }

    #[test]
    fn folding_hides_replies_and_keeps_nested_folds() {
        let mut t = thread();
        t.nodes[1].folded = true;
        assert_eq!(t.visible(), [0, 1, 3, 4]);
        t.nodes[0].folded = true;
        assert_eq!(t.visible(), [0, 4]);
        t.nodes[0].folded = false;
        assert_eq!(t.visible(), [0, 1, 3, 4], "the inner fold survives");
    }

    #[test]
    fn a_reply_for_an_old_feed_is_dropped() {
        let (mut a, _rx) = App::new(Theme::default());
        a.generation = 2;
        a.loading_feed = true;
        a.on_msg(Msg::Feed {
            generation: 1,
            result: Ok(Listing {
                ids: vec![1],
                requested: 1,
                stories: vec![Story::default()],
            }),
        });
        assert!(a.stories.is_empty() && a.loading_feed);
    }

    #[test]
    fn nothing_more_is_requested_past_the_end_of_a_feed() {
        let (mut a, _rx) = App::new(Theme::default());
        a.ids = vec![1, 2, 3];
        a.requested = 3;
        a.load_more();
        assert!(!a.loading_more);
    }

    #[test]
    fn a_failed_page_waits_for_the_user_rather_than_being_asked_for_every_frame() {
        let mut a = loaded(&[1, 2]);
        a.ids = (1..=100).collect();
        a.requested = 60;
        a.loading_more = true;
        a.on_msg(Msg::More {
            generation: a.generation,
            from: 30,
            result: Err("timed out".into()),
        });
        for _ in 0..3 {
            a.clamp_scroll(40);
        }
        assert!(!a.loading_more, "asked again with nobody moving");
        a.move_sel(1);
        assert!(a.loading_more, "a move asks again");
        assert_eq!(a.requested, 60);
    }

    #[test]
    fn a_failed_page_is_asked_for_again_not_skipped() {
        let mut a = loaded(&[1, 2]);
        a.ids = (1..=100).collect();
        a.requested = 60;
        a.loading_more = true;
        a.on_msg(Msg::More {
            generation: a.generation,
            from: 30,
            result: Err("timed out".into()),
        });
        assert_eq!(a.requested, 30);
        assert!(!a.loading_more && a.error.is_some());
    }

    #[test]
    fn a_new_list_keeps_the_cursor_on_the_same_story() {
        let mut a = loaded(&[1, 2, 3, 4]);
        a.select(2);
        a.detail_scroll = 7;
        a.loading_feed = true;
        a.on_msg(Msg::Feed {
            generation: a.generation,
            result: Ok(Listing {
                ids: vec![9, 1, 2, 3],
                requested: 4,
                stories: [9, 1, 2, 3].map(story).to_vec(),
            }),
        });
        assert_eq!(a.selected_story().map(|s| s.id), Some(3));
        assert_eq!(a.detail_scroll, 7, "still reading the same thread");
    }

    #[test]
    fn switching_feeds_forgets_the_story_that_was_settling() {
        let mut a = loaded(&[1, 2, 3]);
        a.select(1);
        a.set_feed(Feed::New);
        a.tick();
        assert!(!a.read.contains(&2) && a.pending.is_empty());
    }

    #[test]
    fn a_feed_opened_before_comes_back_at_once_where_it_was_left() {
        let mut a = loaded(&[1, 2, 3]);
        a.select(2);
        a.set_feed(Feed::New);
        assert!(a.loading_feed && a.stories.is_empty());
        a.set_feed(Feed::Top);
        assert!(!a.loading_feed, "fresh enough: no request");
        assert_eq!(a.selected_story().map(|s| s.id), Some(3));
    }

    #[test]
    fn a_refreshed_thread_keeps_its_folds() {
        let mut a = loaded(&[1]);
        let t = thread();
        let folded = t.nodes[1].id;
        a.threads.insert(1, t);
        a.toggle_fold(1);
        let mut again = thread();
        // Same comments, as a fetch would return them.
        for (n, old) in again.nodes.iter_mut().zip(&a.threads[&1].nodes) {
            n.id = old.id;
        }
        a.on_msg(Msg::Thread {
            id: 1,
            result: Ok(again),
        });
        let t = &a.threads[&1];
        assert!(t.nodes.iter().all(|n| n.folded == (n.id == folded)));
    }

    #[test]
    fn a_failed_fetch_is_that_storys_error_and_goes_when_it_loads() {
        let mut a = loaded(&[1, 2]);
        a.on_msg(Msg::Thread {
            id: 1,
            result: Err("boom".into()),
        });
        assert!(a.error.is_none(), "not the whole feed's failure");
        assert_eq!(a.thread_errors.get(&1).map(String::as_str), Some("boom"));
        a.on_msg(Msg::Thread {
            id: 1,
            result: Ok(thread()),
        });
        assert!(a.thread_errors.is_empty());
    }

    #[test]
    fn the_wheel_scrolls_the_list_without_moving_the_cursor() {
        let mut a = loaded(&(1..=50).collect::<Vec<_>>());
        a.hits.list.height = 10;
        a.scroll_list(5);
        a.clamp_scroll(10);
        assert_eq!((a.selected, a.offset), (0, 5));
        a.scroll_list(100);
        a.clamp_scroll(10);
        assert_eq!(a.offset, 40, "no further than the last row");
        a.move_sel(1);
        a.clamp_scroll(10);
        assert_eq!((a.selected, a.offset), (1, 1), "the keys bring it back");
    }

    #[test]
    fn only_web_links_are_opened() {
        let (mut a, _rx) = App::new(Theme::default());
        for url in [
            "file:///Applications/Calculator.app",
            "-a Terminal",
            "ssh://x",
        ] {
            a.open_url(url);
            let note = a.notice.take().map(|n| n.0).unwrap_or_default();
            assert!(note.starts_with("not opening"), "{url}: {note}");
        }
    }

    // ------------------------------------------------------------- helpers

    use crate::fake_hn::{FakeHn, node};
    use serde_json::{Value, json};

    /// An app showing the first `requested` of `ids` on the Top feed.
    fn loaded_page(ids: &[u64], requested: usize) -> App {
        let (mut a, _rx) = App::new(Theme::default());
        a.loading_feed = true;
        a.on_msg(Msg::Feed {
            generation: a.generation,
            result: Ok(Listing {
                ids: ids.to_vec(),
                requested,
                stories: ids[..requested].iter().map(|&id| story(id)).collect(),
            }),
        });
        a
    }

    fn shown(a: &App) -> Vec<u64> {
        a.stories.iter().map(|s| s.id).collect()
    }

    fn sorted(set: &HashSet<u64>) -> Vec<u64> {
        let mut v: Vec<u64> = set.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// The cursor has rested long enough: its comments are due now.
    fn settle(a: &mut App) {
        if let Some((id, _)) = a.settling {
            a.settling = Some((id, Instant::now()));
        }
        assert!(a.tick(), "nothing was settling");
    }

    fn ago(d: Duration) -> Instant {
        Instant::now()
            .checked_sub(d)
            .expect("the clock goes back that far")
    }

    /// Ids of the comments the fake serves, clear of any story id.
    const COMMENT: u64 = 100_000;

    /// Story `id` on `hn`, with one comment saying `says`.
    fn serve(hn: &FakeHn, id: u64, says: &str) {
        hn.story(id, &format!("story {id}"), &[COMMENT + id]);
        hn.tree(id, json!([node(COMMENT + id, "alice", says, json!([]))]));
    }

    /// A stand-in HN listing `ids` as `feed`, each a story with one comment.
    fn fake(feed: &str, ids: &[u64]) -> FakeHn {
        let hn = FakeHn::start();
        hn.feed(feed, ids);
        for &id in ids {
            serve(&hn, id, "first");
        }
        hn
    }

    fn against(hn: &FakeHn) -> (App, Receiver<Msg>) {
        let client = Client::at(&hn.firebase(), &hn.algolia(), Duration::from_secs(5));
        App::with_client(Theme::default(), client)
    }

    /// Hand the workers' replies to the app, ticking as the event loop does,
    /// until `done` holds.
    fn pump(a: &mut App, rx: &Receiver<Msg>, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(a) {
            assert!(Instant::now() < deadline, "timed out waiting on the app");
            a.tick();
            if let Ok(msg) = rx.recv_timeout(Duration::from_millis(5)) {
                a.on_msg(msg);
            }
        }
    }

    /// Nothing loading, due, or in flight.
    fn idle(a: &App) -> bool {
        !a.loading_feed && !a.loading_more && a.pending.is_empty() && !a.settling()
    }

    /// Comment fetches for story `id`: the tree comes from Algolia.
    fn thread_fetches(hn: &FakeHn, id: u64) -> usize {
        hn.hits(&format!("/algolia/v1/items/{id}"))
    }

    fn item_fetches(hn: &FakeHn, id: u64) -> usize {
        hn.hits(&format!("/v0/item/{id}.json"))
    }

    fn hit(id: u64, title: &str) -> Value {
        json!({
            "objectID": id.to_string(), "title": title, "author": "pg",
            "url": format!("https://example.com/{id}"), "points": 10,
            "num_comments": 0, "created_at_i": 0,
        })
    }

    fn searches(hn: &FakeHn) -> Vec<String> {
        hn.requests()
            .into_iter()
            .filter(|r| r.starts_with("/algolia/v1/search"))
            .collect()
    }

    // ------------------------------------------------ loading and comments

    #[test]
    fn the_first_load_lists_the_feed_and_fetches_the_first_storys_comments_once() {
        let ids: Vec<u64> = (1..=10).collect();
        let hn = fake("topstories", &ids);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        assert!(a.loading_feed && a.busy() && a.stories.is_empty());
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        assert_eq!(shown(&a), ids);
        assert!(!a.loading_feed && a.error.is_none() && a.last_fetch > 0);

        pump(&mut a, &rx, idle);
        let t = a.thread().expect("the first story's comments");
        assert_eq!(t.nodes[0].by, "alice");
        assert!(a.read.contains(&1));
        // Ticking on does nothing more: the comments are here and fresh.
        for _ in 0..20 {
            assert!(!a.tick());
        }
        let fetches: Vec<usize> = ids.iter().map(|&id| thread_fetches(&hn, id)).collect();
        // The story itself, and the three after it ahead of time.
        assert_eq!(fetches, [1, 1, 1, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(hn.hits("/v0/topstories.json"), 1);
    }

    #[test]
    fn running_down_the_list_fetches_comments_only_where_the_cursor_rests() {
        let ids: Vec<u64> = (1..=30).collect();
        let hn = fake("topstories", &ids);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        for _ in 0..10 {
            a.move_sel(1);
        }
        assert!(a.pending.is_empty(), "nothing fetched while moving");
        pump(&mut a, &rx, idle);
        let fetched: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|&id| thread_fetches(&hn, id) > 0)
            .collect();
        // Story 11 under the cursor, 12–14 ahead of it and 10 behind.
        assert_eq!(fetched, [10, 11, 12, 13, 14]);
        assert!(fetched.iter().all(|&id| thread_fetches(&hn, id) == 1));
    }

    #[test]
    fn the_cursor_has_to_rest_for_the_settle_delay_before_comments_are_fetched() {
        let mut a = loaded(&[1, 2, 3]);
        let before = Instant::now();
        a.select(1);
        let at = a.settle_at().expect("settling");
        assert!(at >= before + SETTLE && at <= Instant::now() + SETTLE);
        a.settling = Some((2, Instant::now() + Duration::from_secs(3600)));
        assert!(!a.tick(), "not due yet");
        assert!(a.pending.is_empty() && !a.read.contains(&2));
        settle(&mut a);
        assert!(a.read.contains(&2) && a.pending.contains(&2));
        assert!(!a.settling() && !a.tick(), "due once");
    }

    #[test]
    fn prefetching_takes_three_stories_ahead_and_one_behind() {
        let mut a = loaded(&(1..=20).collect::<Vec<_>>());
        a.select(10);
        settle(&mut a);
        assert_eq!(sorted(&a.pending), [10, 11, 12, 13, 14]);
    }

    #[test]
    fn prefetching_holds_off_once_enough_fetches_are_in_flight() {
        let mut a = loaded(&(1..=20).collect::<Vec<_>>());
        a.pending.extend([901, 902, 903]);
        a.select(10);
        settle(&mut a);
        // The story being read always goes; its neighbours up to the cap.
        assert_eq!(sorted(&a.pending), [11, 12, 13, 901, 902, 903]);
        assert_eq!(a.pending.len(), MAX_PENDING);

        let mut a = loaded(&(1..=20).collect::<Vec<_>>());
        a.pending.extend(901..=906);
        a.select(10);
        settle(&mut a);
        assert_eq!(a.pending.len(), MAX_PENDING + 1, "just the story read");
        assert!(a.pending.contains(&11));
    }

    #[test]
    fn prefetching_skips_stories_whose_comments_are_here_or_failed() {
        let mut a = loaded(&(1..=10).collect::<Vec<_>>());
        // Even an old copy: it is fetched again when read, not ahead of time.
        let mut old = thread();
        old.fetched = ago(THREAD_TTL + Duration::from_secs(1));
        a.threads.insert(6, old);
        a.thread_errors.insert(4, "boom".into());
        a.select(4);
        settle(&mut a);
        assert_eq!(sorted(&a.pending), [5, 7, 8]);
    }

    #[test]
    fn reading_a_story_whose_comments_failed_asks_for_them_again() {
        let mut a = loaded(&[1, 2]);
        a.thread_errors.insert(1, "timed out".into());
        a.select(0);
        settle(&mut a);
        assert!(a.pending.contains(&1));
    }

    #[test]
    fn the_next_page_is_asked_for_when_the_cursor_comes_within_ten_rows_of_the_end() {
        let mut a = loaded_page(&(1..=100).collect::<Vec<_>>(), 30);
        a.select(19);
        assert!(!a.loading_more);
        a.select(20);
        assert!(a.loading_more);
        assert_eq!(a.requested, 60);
    }

    #[test]
    fn the_next_page_is_asked_for_when_the_wheel_brings_the_end_near() {
        let mut a = loaded_page(&(1..=100).collect::<Vec<_>>(), 30);
        a.hits.list.height = 10;
        a.scroll_list(9);
        assert!(!a.loading_more);
        a.scroll_list(1);
        assert!(a.loading_more);
        assert_eq!(a.requested, 60);
        assert_eq!(a.selected, 0, "the cursor stays put");
    }

    #[test]
    fn a_taller_pane_pulls_a_list_scrolled_to_its_end_back_into_range() {
        let mut a = loaded(&(1..=50).collect::<Vec<_>>());
        a.hits.list.height = 10;
        a.scroll_list(100);
        a.clamp_scroll(10);
        assert_eq!(a.offset, 40);
        a.clamp_scroll(30);
        assert_eq!(a.offset, 20, "no blank rows under the last story");
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn the_next_page_arrives_after_the_first_in_feed_order() {
        let ids: Vec<u64> = (1..=100).collect();
        let hn = fake("topstories", &ids);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        assert_eq!(a.stories.len(), PAGE);
        a.select(25);
        pump(&mut a, &rx, |a| !a.loading_more);
        assert_eq!(shown(&a), (1..=60).collect::<Vec<_>>());
        assert!((61..=100).all(|id| item_fetches(&hn, id) == 0));
    }

    #[test]
    fn a_list_pane_taller_than_the_list_keeps_asking_until_it_has_rows_to_spare() {
        let hn = fake("topstories", &(1..=100).collect::<Vec<_>>());
        let (mut a, rx) = against(&hn);
        a.hits.list.height = 50;
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_feed && !a.loading_more);
        // 60 would leave fewer than ten rows past the pane; 90 does not.
        assert_eq!((a.stories.len(), a.requested), (90, 90));
        assert_eq!(a.selected, 0, "without the cursor moving");
    }

    #[test]
    fn a_page_that_failed_is_asked_for_again_rather_than_skipped() {
        let ids: Vec<u64> = (1..=60).collect();
        let hn = fake("topstories", &ids);
        for id in 31..=60 {
            hn.route(&format!("/v0/item/{id}.json"), 500, "down");
        }
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(25);
        pump(&mut a, &rx, |a| !a.loading_more);
        assert!(a.error.is_some());
        assert_eq!((a.stories.len(), a.requested), (30, 30));

        for id in 31..=60 {
            serve(&hn, id, "first");
        }
        a.move_sel(1);
        pump(&mut a, &rx, |a| !a.loading_more);
        assert_eq!(shown(&a), ids);
        assert!(a.error.is_none());
    }

    #[test]
    fn a_page_still_on_its_way_when_the_tab_changes_is_asked_for_again_on_return() {
        let ids: Vec<u64> = (1..=100).collect();
        let hn = fake("topstories", &ids);
        hn.feed("newstories", &[]);
        for id in 31..=60 {
            hn.delay(&format!("/v0/item/{id}.json"), Duration::from_millis(200));
        }
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(25);
        assert!(a.loading_more);
        // The page's reply now comes back for a feed that was left: dropped.
        a.set_feed(Feed::New);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_more);
        assert_eq!(shown(&a), (1..=60).collect::<Vec<_>>());
    }

    #[test]
    fn a_feed_left_and_reopened_within_its_ttl_comes_back_without_a_request() {
        let hn = fake("topstories", &[1, 2, 3, 4, 5]);
        hn.feed("newstories", &[11, 12]);
        serve(&hn, 11, "first");
        serve(&hn, 12, "first");
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(3);
        a.set_feed(Feed::New);
        assert!(a.loading_feed && a.stories.is_empty());
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        assert_eq!(shown(&a), [11, 12]);

        // Not quite old enough to fetch again.
        a.feeds.get_mut(&Feed::Top).unwrap().fetched -= FEED_TTL - 5;
        a.set_feed(Feed::Top);
        assert!(!a.loading_feed, "no request");
        assert_eq!(shown(&a), [1, 2, 3, 4, 5]);
        assert_eq!(a.selected_story().map(|s| s.id), Some(4));
        a.set_feed(Feed::New);
        assert_eq!(shown(&a), [11, 12]);
        pump(&mut a, &rx, idle);
        assert_eq!(hn.hits("/v0/topstories.json"), 1);
        assert_eq!(hn.hits("/v0/newstories.json"), 1);
    }

    #[test]
    fn a_feed_reopened_after_its_ttl_shows_at_once_and_is_fetched_again_behind_it() {
        let hn = fake("topstories", &[1, 2, 3, 4, 5]);
        hn.feed("newstories", &[11]);
        serve(&hn, 11, "first");
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(3);
        a.set_feed(Feed::New);
        pump(&mut a, &rx, |a| !a.stories.is_empty());

        a.feeds.get_mut(&Feed::Top).unwrap().fetched -= FEED_TTL + 5;
        serve(&hn, 9, "first");
        hn.feed("topstories", &[9, 4, 1, 2, 3, 5]);
        a.set_feed(Feed::Top);
        assert!(a.loading_feed);
        assert_eq!(shown(&a), [1, 2, 3, 4, 5], "the old copy meanwhile");
        assert_eq!(a.selected_story().map(|s| s.id), Some(4));
        pump(&mut a, &rx, |a| !a.loading_feed);
        assert_eq!(shown(&a), [9, 4, 1, 2, 3, 5]);
        assert_eq!(a.selected_story().map(|s| s.id), Some(4));
        assert_eq!(hn.hits("/v0/topstories.json"), 2);
    }

    #[test]
    fn refresh_keeps_the_list_up_and_the_cursor_on_its_story_when_the_order_changes() {
        let hn = fake("topstories", &[1, 2, 3, 4, 5, 6]);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(3);
        pump(&mut a, &rx, idle);
        a.detail_scroll = 4;

        hn.feed("topstories", &[6, 5, 4, 3, 2, 1]);
        a.refresh();
        assert!(a.loading_feed);
        assert_eq!(
            shown(&a),
            [1, 2, 3, 4, 5, 6],
            "kept until the new list lands"
        );
        pump(&mut a, &rx, idle);
        assert_eq!(shown(&a), [6, 5, 4, 3, 2, 1]);
        assert_eq!(a.selected_story().map(|s| s.id), Some(4));
        assert_eq!(a.detail_scroll, 4, "still reading the same thread");
        assert_eq!(hn.hits("/v0/topstories.json"), 2);
    }

    #[test]
    fn refresh_fetches_the_thread_being_read_again_and_keeps_its_folds() {
        let hn = FakeHn::start();
        hn.feed("topstories", &[1]);
        hn.story(1, "story 1", &[10, 20]);
        let reply = node(11, "bob", "reply", json!([]));
        hn.tree(
            1,
            json!([
                node(10, "alice", "one", json!([reply.clone()])),
                node(20, "carol", "two", json!([])),
            ]),
        );
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| a.thread().is_some() && idle(a));
        a.toggle_fold(0);

        // A new reply turns up under the folded comment.
        let fresh = node(12, "dave", "new", json!([]));
        hn.tree(
            1,
            json!([
                node(10, "alice", "one", json!([reply, fresh])),
                node(20, "carol", "two", json!([])),
            ]),
        );
        a.refresh();
        assert!(a.pending.contains(&1), "fetched again at once");
        assert!(a.thread().is_some(), "the old copy stays meanwhile");
        pump(&mut a, &rx, idle);
        let t = a.thread().unwrap();
        let ids: Vec<u64> = t.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, [10, 11, 12, 20], "the fresh copy");
        let folded: Vec<u64> = t.nodes.iter().filter(|n| n.folded).map(|n| n.id).collect();
        assert_eq!(folded, [10]);
        assert_eq!(thread_fetches(&hn, 1), 2);
    }

    #[test]
    fn after_refresh_a_cached_thread_is_shown_while_it_is_fetched_again() {
        let hn = fake("topstories", &[1, 2, 3]);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| a.threads.len() == 3 && idle(a));

        hn.story(2, "story 2, edited", &[COMMENT + 2]);
        a.refresh();
        pump(&mut a, &rx, idle);
        assert_eq!(
            thread_fetches(&hn, 2),
            1,
            "prefetching leaves old copies be"
        );

        a.select(1);
        settle(&mut a);
        assert!(a.pending.contains(&2));
        let title = |a: &App| a.thread().map(|t| t.story.title.clone());
        assert_eq!(
            title(&a).as_deref(),
            Some("story 2"),
            "the old copy meanwhile"
        );
        pump(&mut a, &rx, idle);
        assert_eq!(title(&a).as_deref(), Some("story 2, edited"));
        assert_eq!(thread_fetches(&hn, 2), 2);
    }

    #[test]
    fn a_thread_past_its_ttl_is_fetched_again_when_read_and_shown_meanwhile() {
        let mut a = loaded(&[1, 2]);
        // Not out of date by a refresh: by age alone.
        a.stale_before = ago(THREAD_TTL * 2);
        let mut old = thread();
        old.fetched = ago(THREAD_TTL + Duration::from_secs(1));
        a.threads.insert(1, old);
        a.threads.insert(2, thread());
        a.select(0);
        settle(&mut a);
        assert!(a.pending.contains(&1) && a.thread().is_some());
        a.select(1);
        settle(&mut a);
        assert!(!a.pending.contains(&2), "a fresh copy is not fetched again");
    }

    #[test]
    fn past_64_threads_the_oldest_goes_but_never_the_one_being_read() {
        let mut a = loaded(&(1..=70).collect::<Vec<_>>());
        let base = ago(Duration::from_secs(60));
        for id in 1..KEEP_THREADS as u64 {
            let mut t = thread();
            t.fetched = base + Duration::from_millis(id);
            a.threads.insert(id, t);
        }
        a.select(0); // story 1, the oldest copy
        a.on_msg(Msg::Thread {
            id: 64,
            result: Ok(thread()),
        });
        assert_eq!(a.threads.len(), KEEP_THREADS, "64 fit");
        a.on_msg(Msg::Thread {
            id: 65,
            result: Ok(thread()),
        });
        assert_eq!(a.threads.len(), KEEP_THREADS);
        assert!(a.threads.contains_key(&1), "being read");
        assert!(!a.threads.contains_key(&2), "the oldest of the rest");
        assert!(a.threads.contains_key(&65));
    }

    #[test]
    fn a_failed_refetch_keeps_the_copy_on_screen_and_shows_no_error() {
        let mut a = loaded(&[1]);
        a.threads.insert(1, thread());
        a.pending.insert(1);
        a.on_msg(Msg::Thread {
            id: 1,
            result: Err("timed out".into()),
        });
        assert!(a.thread().is_some() && a.pending.is_empty());
        assert!(a.thread_errors.is_empty() && a.error.is_none());
    }

    #[test]
    fn refresh_forgets_comment_errors() {
        let mut a = loaded(&[1, 2]);
        a.thread_errors.insert(2, "boom".into());
        a.refresh();
        assert!(a.thread_errors.is_empty());
    }

    #[test]
    fn a_page_for_a_feed_since_fetched_again_is_dropped() {
        let mut a = loaded_page(&(1..=100).collect::<Vec<_>>(), 30);
        a.select(25);
        let old = a.generation;
        a.refresh();
        a.on_msg(Msg::More {
            generation: old,
            from: 30,
            result: Ok((31..=60).map(story).collect()),
        });
        assert_eq!(a.stories.len(), 30);
        a.on_msg(Msg::More {
            generation: old,
            from: 30,
            result: Err("late".into()),
        });
        assert!(a.error.is_none() && a.loading_feed);
    }

    #[test]
    fn a_slow_feed_answering_after_a_tab_switch_is_dropped() {
        let hn = fake("topstories", &[1, 2]);
        hn.feed("newstories", &[11, 12]);
        serve(&hn, 11, "first");
        serve(&hn, 12, "first");
        hn.delay("/v0/topstories.json", Duration::from_millis(300));
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        let left = a.generation;
        a.set_feed(Feed::New);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut late = false;
        while !(late && !a.stories.is_empty()) {
            assert!(Instant::now() < deadline, "timed out");
            if let Ok(msg) = rx.recv_timeout(Duration::from_millis(5)) {
                late |= matches!(msg, Msg::Feed { generation, .. } if generation == left);
                a.on_msg(msg);
            }
        }
        assert_eq!(a.feed, Feed::New);
        assert_eq!(shown(&a), [11, 12]);
    }

    #[test]
    fn searching_from_a_feed_opens_the_search_tab_and_keeps_the_feed_to_go_back_to() {
        let hn = fake("topstories", &[1, 2, 3]);
        hn.search(json!([hit(77, "Rust 2.0"), hit(78, "Rust in space")]));
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        a.select(2);
        a.search("rust".into());
        assert_eq!(a.feed, Feed::Search);
        assert_eq!(a.tabs().last(), Some(&Feed::Search));
        pump(&mut a, &rx, |a| !a.loading_feed);
        assert_eq!(shown(&a), [77, 78]);
        assert_eq!(a.selected, 0);
        let asked = searches(&hn);
        assert!(
            asked.len() == 1 && asked[0].contains("query=rust"),
            "{asked:?}"
        );

        a.set_feed(Feed::Top);
        assert!(!a.loading_feed, "the feed was kept");
        assert_eq!(a.selected_story().map(|s| s.id), Some(3));
    }

    #[test]
    fn searching_again_from_the_search_tab_replaces_the_results() {
        let hn = fake("topstories", &[1]);
        hn.search(json!([hit(77, "Rust 2.0"), hit(78, "Rust in space")]));
        let (mut a, rx) = against(&hn);
        a.search("rust".into());
        pump(&mut a, &rx, |a| !a.loading_feed);
        a.select(1);

        hn.search(json!([hit(90, "Zig")]));
        a.search("zig".into());
        assert!(a.loading_feed && a.stories.is_empty(), "the old results go");
        pump(&mut a, &rx, |a| !a.loading_feed);
        assert_eq!(shown(&a), [90]);
        assert_eq!(a.query, "zig");
        assert!(searches(&hn)[1].contains("query=zig"));

        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_feed);
        a.set_feed(Feed::Search);
        assert!(!a.loading_feed);
        assert_eq!(shown(&a), [90]);
    }

    #[test]
    fn a_new_search_from_another_tab_does_not_bring_back_the_old_results() {
        let hn = fake("topstories", &[1]);
        hn.search(json!([hit(77, "Rust 2.0")]));
        let (mut a, rx) = against(&hn);
        a.search("rust".into());
        pump(&mut a, &rx, |a| !a.loading_feed);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_feed);

        hn.search(json!([hit(90, "Zig")]));
        a.search("zig".into());
        assert!(a.loading_feed && a.stories.is_empty());
        pump(&mut a, &rx, |a| !a.loading_feed);
        assert_eq!(shown(&a), [90]);
    }

    #[test]
    fn dead_deleted_and_missing_stories_are_left_out_of_the_list() {
        let hn = FakeHn::start();
        hn.feed("topstories", &[1, 2, 3, 4, 5]);
        hn.story(1, "alive", &[]);
        hn.item(json!({"id": 2, "type": "story", "title": "dead", "dead": true}));
        hn.route("/v0/item/3.json", 200, "null");
        hn.item(json!({"id": 4, "type": "story", "deleted": true}));
        hn.story(5, "also alive", &[]);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_feed);
        assert_eq!(shown(&a), [1, 5]);
        assert!(a.error.is_none());
    }

    #[test]
    fn a_first_page_of_only_dead_stories_goes_on_to_the_next() {
        let hn = FakeHn::start();
        let ids: Vec<u64> = (1..=40).collect();
        hn.feed("topstories", &ids);
        for id in 1..=30 {
            hn.item(json!({"id": id, "type": "story", "dead": true}));
        }
        for id in 31..=40 {
            serve(&hn, id, "first");
        }
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.loading_feed && !a.loading_more);
        assert_eq!(shown(&a), (31..=40).collect::<Vec<_>>());
        // The story under the cursor has its comments on the way.
        assert!(a.settling() || a.pending.contains(&31) || a.threads.contains_key(&31));
    }

    #[test]
    fn a_story_gone_since_it_was_listed_is_that_storys_error_alone() {
        let hn = fake("topstories", &[1, 2]);
        let (mut a, rx) = against(&hn);
        a.set_feed(Feed::Top);
        pump(&mut a, &rx, |a| !a.stories.is_empty());
        hn.route("/v0/item/1.json", 200, "null");
        pump(&mut a, &rx, idle);
        let e = a.thread_errors.get(&1).expect("an error for story 1");
        assert!(e.contains("no longer exists"), "{e}");
        assert!(a.error.is_none(), "not the feed's");
        assert!(a.threads.contains_key(&2) && !a.thread_errors.contains_key(&2));
    }

    /// Random sessions: keys, the mouse, resizes and worker replies in any
    /// order, drawn as they go and checked throughout.
    mod fuzz {
        use crate::app::{App, Listing, Msg, PAGE, Thread};
        use crate::event::Input;
        use crate::hn::{Comment, Feed, Story};
        use crate::theme::Theme;
        use crate::ui;
        use crate::util::now_secs;
        use proptest::prelude::*;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::buffer::Buffer;
        use ratatui::crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
            MouseEventKind,
        };
        use std::collections::HashSet;
        use std::sync::mpsc::Receiver;
        use std::time::{Duration, Instant};

        /// Ids the stand-in server's lists are drawn from, so feeds overlap.
        const POOL: usize = 150;

        #[derive(Debug, Clone)]
        struct Server {
            lens: [usize; 6],
            rot: usize,
        }

        impl Server {
            fn ids(&self, feed: Feed, query: &str) -> Vec<u64> {
                let id = |n: usize| 1 + (n % POOL) as u64;
                match Feed::TABS.iter().position(|f| *f == feed) {
                    Some(fi) => (0..self.lens[fi])
                        .map(|i| id(i + self.rot + 7 * fi))
                        .collect(),
                    None => {
                        let h = query
                            .bytes()
                            .fold(self.rot, |h, b| (h * 31 + b as usize) % POOL);
                        (0..h % 40).map(|i| id(3 * i + h)).collect()
                    }
                }
            }
        }

        /// Stories the server lists but no longer serves.
        fn dead(id: u64) -> bool {
            id % 5 == 3
        }

        fn story(id: u64) -> Story {
            Story {
                id,
                title: match id % 4 {
                    0 => format!("日本語のタイトル {id}"),
                    1 => format!("a title that goes on well past the width of any pane {id}"),
                    2 => String::new(),
                    _ => format!("story {id}"),
                },
                by: format!("user{}", id % 7),
                score: (id * 37 % 500) as u32,
                time: now_secs() - id as i64 * 600,
                comments: (id % 9) as u32,
                url: (!id.is_multiple_of(3)).then(|| format!("https://example.com/{id}")),
                text: id
                    .is_multiple_of(6)
                    .then(|| "a <i>text</i> post<p>in two paragraphs".into()),
                kids: Vec::new(),
            }
        }

        fn comments(id: u64, roots: usize) -> Vec<Comment> {
            let c = |cid: u64, depth: usize, text: String, children: Vec<Comment>| Comment {
                id: cid,
                by: format!("u{}", cid % 5),
                time: now_secs() - 60,
                text,
                depth,
                children,
            };
            (0..roots as u64)
                .map(|r| {
                    let base = id * 1000 + r * 10;
                    let replies = (0..r % 3)
                        .map(|k| {
                            let deeper = (k == 1)
                                .then(|| c(base + 5, 2, "deeper".into(), Vec::new()))
                                .into_iter()
                                .collect();
                            c(base + 1 + k, 1, "a reply".into(), deeper)
                        })
                        .collect();
                    let text = if r % 2 == 0 {
                        "words ".repeat(1 + r as usize * 3)
                    } else {
                        format!("see <a href=\"https://x.example/{r}\">this</a> and <code>x</code>")
                    };
                    c(base, 0, text, replies)
                })
                .collect()
        }

        #[derive(Debug, Clone)]
        enum Op {
            Key(KeyCode, KeyModifiers, KeyEventKind),
            Mouse(MouseEventKind, u16, u16),
            /// Notches of the wheel over the list, or over the comments.
            Wheel {
                list: bool,
                down: bool,
                notches: u8,
            },
            /// A left click on the `zone`th thing the last frame drew;
            /// `twice` for a double-click.
            Click {
                zone: usize,
                twice: bool,
            },
            Resize(u16, u16),
            Draw,
            /// The cursor's settle delay runs out and the loop ticks.
            Settle,
            /// The answer to the feed request in flight: success or failure.
            Feed(bool),
            More(bool),
            /// The answer to one of the comment fetches in flight.
            Thread {
                pick: usize,
                ok: bool,
                roots: usize,
            },
            /// An answer for a feed since switched away from or fetched again.
            Stale {
                more: bool,
                len: usize,
            },
            /// The server's lists change order, as a refresh then finds.
            Shuffle(usize),
        }

        fn key() -> impl Strategy<Value = Op> {
            use KeyCode::*;
            let bound: Vec<KeyCode> = "jkgGJK bnpxzocr/?q1234567]\\"
                .chars()
                .map(Char)
                .chain([
                    Enter, Esc, Tab, BackTab, Up, Down, Home, End, PageUp, PageDown, Backspace,
                ])
                .collect();
            let code = prop_oneof![
                6 => prop::sample::select(bound),
                1 => any::<char>().prop_filter("typed", |c| !c.is_control()).prop_map(Char),
            ];
            let mods = prop::sample::select(vec![
                KeyModifiers::NONE,
                KeyModifiers::NONE,
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ]);
            let kind = prop_oneof![
                8 => Just(KeyEventKind::Press),
                1 => Just(KeyEventKind::Repeat),
                1 => Just(KeyEventKind::Release),
            ];
            (code, mods, kind).prop_map(|(c, m, k)| Op::Key(c, m, k))
        }

        fn mouse() -> impl Strategy<Value = Op> {
            use MouseEventKind::*;
            let kind = prop::sample::select(vec![
                Down(MouseButton::Left),
                Down(MouseButton::Right),
                Up(MouseButton::Left),
                Drag(MouseButton::Left),
                Moved,
                ScrollUp,
                ScrollDown,
                ScrollDown,
                ScrollLeft,
            ]);
            (kind, 0u16..240, 0u16..80).prop_map(|(k, x, y)| Op::Mouse(k, x, y))
        }

        fn size() -> impl Strategy<Value = (u16, u16)> {
            prop_oneof![
                1 => Just((1, 1)),
                1 => Just((20, 6)),
                1 => Just((12, 3)),
                2 => (1u16..=40, 1u16..=12),
                6 => (40u16..=220, 5u16..=70),
            ]
        }

        fn reply() -> impl Strategy<Value = Op> {
            prop_oneof![
                3 => prop::bool::weighted(0.8).prop_map(Op::Feed),
                2 => prop::bool::weighted(0.8).prop_map(Op::More),
                3 => (any::<usize>(), prop::bool::weighted(0.8), 0usize..8)
                    .prop_map(|(pick, ok, roots)| Op::Thread { pick, ok, roots }),
                1 => (any::<bool>(), 0usize..40).prop_map(|(more, len)| Op::Stale { more, len }),
            ]
        }

        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                10 => key(),
                3 => mouse(),
                3 => (any::<bool>(), prop::bool::weighted(0.7), 1u8..6)
                    .prop_map(|(list, down, notches)| Op::Wheel { list, down, notches }),
                4 => (any::<usize>(), any::<bool>())
                    .prop_map(|(zone, twice)| Op::Click { zone, twice }),
                2 => size().prop_map(|(w, h)| Op::Resize(w, h)),
                4 => Just(Op::Draw),
                2 => Just(Op::Settle),
                9 => reply(),
                1 => (1usize..50).prop_map(Op::Shuffle),
            ]
        }

        fn server() -> impl Strategy<Value = Server> {
            let len = prop_oneof![
                Just(0usize),
                1usize..=45,
                Just(PAGE),
                Just(2 * PAGE),
                61usize..=120,
            ];
            (prop::array::uniform6(len), 0..POOL).prop_map(|(lens, rot)| Server { lens, rot })
        }

        /// Which invariants a run holds the app to.
        #[derive(Clone, Copy, Default)]
        struct Checks {
            /// Selection, list and scroll positions, requests, stale replies.
            view: bool,
            /// Every click target on screen, and none on top of another.
            zones: bool,
            /// Every story asked for is shown, unless it is dead.
            pages: bool,
        }

        struct Session {
            app: App,
            rx: Receiver<Msg>,
            /// Worker answers taken off the channel but not yet handed over.
            inbox: Vec<Msg>,
            input: Input,
            size: (u16, u16),
            server: Server,
            /// Every story that has been in the list.
            seen: HashSet<u64>,
            checks: Checks,
        }

        impl Session {
            fn new(size: (u16, u16), server: Server, checks: Checks) -> Self {
                let (mut app, rx) = App::new(Theme::default());
                // A program that isn't there fails at once: nothing to launch.
                app.opener = "/nonexistent/hackney-opener".into();
                app.set_feed(Feed::Top);
                Session {
                    app,
                    rx,
                    inbox: Vec::new(),
                    input: Input::default(),
                    size,
                    server,
                    seen: HashSet::new(),
                    checks,
                }
            }

            /// The answer a worker sent that `want` picks out, waiting for it
            /// if it isn't in yet. The app's own workers fail at once (they
            /// have nowhere to connect); their answers are replaced with what
            /// the run decides, so the outcome doesn't depend on timing.
            fn answer(&mut self, want: impl Fn(&Msg) -> bool) -> Msg {
                if let Some(i) = self.inbox.iter().position(&want) {
                    return self.inbox.swap_remove(i);
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let msg = self
                        .rx
                        .recv_timeout(left)
                        .expect("a request was never answered");
                    if want(&msg) {
                        return msg;
                    }
                    self.inbox.push(msg);
                }
            }

            fn draw(&mut self) -> Buffer {
                let (w, h) = self.size;
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| ui::draw(f, &mut self.app)).unwrap();
                term.backend().buffer().clone()
            }

            fn handle(&mut self, ev: Event) {
                self.input.handle(&mut self.app, ev);
            }

            /// The server's answer to the feed request in flight.
            fn listing(&self) -> Listing {
                let a = &self.app;
                let ids = self.server.ids(a.feed, &a.query);
                if a.feed == Feed::Search {
                    let stories = ids.iter().map(|&id| story(id)).collect();
                    return Listing {
                        requested: ids.len(),
                        ids,
                        stories,
                    };
                }
                // A first page, or as many as were loaded, for a refresh.
                let requested = ids.len().min(a.requested.max(PAGE));
                Listing {
                    stories: ids[..requested]
                        .iter()
                        .copied()
                        .filter(|&id| !dead(id))
                        .map(story)
                        .collect(),
                    requested,
                    ids,
                }
            }

            fn apply(&mut self, op: &Op) {
                self.note_seen();
                match *op {
                    Op::Key(code, mods, kind) => {
                        self.handle(Event::Key(KeyEvent::new_with_kind(code, mods, kind)))
                    }
                    Op::Mouse(kind, column, row) => self.handle(Event::Mouse(MouseEvent {
                        kind,
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    })),
                    Op::Wheel {
                        list,
                        down,
                        notches,
                    } => {
                        let h = &self.app.hits;
                        let r = if list { h.list } else { h.detail };
                        let kind = if down {
                            MouseEventKind::ScrollDown
                        } else {
                            MouseEventKind::ScrollUp
                        };
                        for _ in 0..notches {
                            self.handle(Event::Mouse(MouseEvent {
                                kind,
                                column: r.x + r.width / 2,
                                row: r.y + r.height / 2,
                                modifiers: KeyModifiers::NONE,
                            }));
                        }
                    }
                    Op::Click { zone, twice } => {
                        // As the event loop does: a click waits for the frame it was aimed at.
                        self.draw();
                        let zones = &self.app.hits.zones;
                        let Some(&(r, _)) = zones.get(zone % zones.len().max(1)) else {
                            return;
                        };
                        let click = Event::Mouse(MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: r.x,
                            row: r.y,
                            modifiers: KeyModifiers::NONE,
                        });
                        self.handle(click.clone());
                        if twice {
                            self.draw();
                            self.handle(click);
                        }
                    }
                    Op::Resize(w, h) => {
                        self.size = (w, h);
                        self.handle(Event::Resize(w, h));
                    }
                    Op::Draw => {
                        let buf = self.draw();
                        self.check_frame(&buf);
                    }
                    Op::Settle => {
                        if let Some((id, _)) = self.app.settling {
                            self.app.settling = Some((id, Instant::now()));
                        }
                        self.app.tick();
                    }
                    Op::Feed(ok) => {
                        if !self.app.loading_feed {
                            return;
                        }
                        let g = self.app.generation;
                        self.answer(
                            |m| matches!(m, Msg::Feed { generation, .. } if *generation == g),
                        );
                        let result = if ok {
                            Ok(self.listing())
                        } else {
                            Err("the feed failed".into())
                        };
                        self.app.on_msg(Msg::Feed {
                            generation: g,
                            result,
                        });
                    }
                    Op::More(ok) => {
                        if !self.app.loading_more {
                            return;
                        }
                        let g = self.app.generation;
                        let Msg::More { from, .. } = self.answer(
                            |m| matches!(m, Msg::More { generation, .. } if *generation == g),
                        ) else {
                            unreachable!()
                        };
                        let a = &self.app;
                        assert!(from < a.requested && a.requested - from <= PAGE);
                        let result = if ok {
                            Ok(a.ids[from..a.requested]
                                .iter()
                                .copied()
                                .filter(|&id| !dead(id))
                                .map(story)
                                .collect())
                        } else {
                            Err("the page failed".into())
                        };
                        self.app.on_msg(Msg::More {
                            generation: g,
                            from,
                            result,
                        });
                    }
                    Op::Thread { pick, ok, roots } => {
                        let mut ids: Vec<u64> = self.app.pending.iter().copied().collect();
                        if ids.is_empty() {
                            return;
                        }
                        ids.sort_unstable();
                        let id = ids[pick % ids.len()];
                        self.answer(|m| matches!(m, Msg::Thread { id: i, .. } if *i == id));
                        let result = if ok {
                            Ok(Thread::new(story(id), comments(id, roots)))
                        } else {
                            Err("the comments failed".into())
                        };
                        self.app.on_msg(Msg::Thread { id, result });
                    }
                    Op::Stale { more, len } => {
                        let Some(g) = self.app.generation.checked_sub(1) else {
                            return;
                        };
                        let before = self.snapshot();
                        let stories: Vec<Story> = (500..500 + len as u64).map(story).collect();
                        self.app.on_msg(if more {
                            Msg::More {
                                generation: g,
                                from: 0,
                                result: Ok(stories),
                            }
                        } else {
                            Msg::Feed {
                                generation: g,
                                result: Ok(Listing {
                                    ids: stories.iter().map(|s| s.id).collect(),
                                    requested: len,
                                    stories,
                                }),
                            }
                        });
                        if self.checks.view {
                            assert!(self.snapshot() == before, "a stale answer changed the app");
                        }
                    }
                    Op::Shuffle(by) => self.server.rot += by,
                }
                self.note_seen();
                self.check_state();
            }

            fn note_seen(&mut self) {
                self.seen.extend(self.app.stories.iter().map(|s| s.id));
            }

            #[allow(clippy::type_complexity)]
            fn snapshot(&self) -> (Vec<u64>, Vec<u64>, usize, bool, bool, Option<String>, usize) {
                let a = &self.app;
                (
                    a.ids.clone(),
                    a.stories.iter().map(|s| s.id).collect(),
                    a.requested,
                    a.loading_feed,
                    a.loading_more,
                    a.error.clone(),
                    a.selected,
                )
            }

            /// Answer everything still in flight, successfully, as long as
            /// the app keeps asking.
            fn finish(&mut self) {
                for _ in 0..1000 {
                    let op = if self.app.loading_feed {
                        Op::Feed(true)
                    } else if self.app.loading_more {
                        Op::More(true)
                    } else if !self.app.pending.is_empty() {
                        Op::Thread {
                            pick: 0,
                            ok: true,
                            roots: 3,
                        }
                    } else {
                        let buf = self.draw();
                        self.check_frame(&buf);
                        return;
                    };
                    self.apply(&op);
                }
                panic!("the app never stopped asking");
            }

            fn check_state(&self) {
                let a = &self.app;
                if self.checks.view {
                    let n = a.stories.len();
                    assert!(a.selected < n.max(1), "selected {} of {n}", a.selected);
                    assert!(
                        a.requested <= a.ids.len(),
                        "{} of {} asked for",
                        a.requested,
                        a.ids.len()
                    );
                    // The list is the feed in its order, less what is dead.
                    let mut rest = a.ids[..a.requested].iter();
                    for s in &a.stories {
                        assert!(rest.any(|&id| id == s.id), "story {} is out of place", s.id);
                    }
                    for id in &a.pending {
                        assert!(
                            self.seen.contains(id),
                            "comments for {id}, never listed, asked for"
                        );
                    }
                }
                if self.checks.pages && !a.loading_more && !a.loading_feed {
                    let shown: HashSet<u64> = a.stories.iter().map(|s| s.id).collect();
                    let search = a.feed == Feed::Search;
                    for &id in &a.ids[..a.requested] {
                        assert!(
                            shown.contains(&id) || (!search && dead(id)),
                            "story {id} was asked for but never listed: {:?} of {:?}",
                            a.stories.iter().map(|s| s.id).collect::<Vec<_>>(),
                            &a.ids[..a.requested],
                        );
                    }
                }
            }

            fn check_frame(&mut self, _buf: &Buffer) {
                let (w, h) = self.size;
                if self.checks.zones {
                    let zones: Vec<_> = self
                        .app
                        .hits
                        .zones
                        .iter()
                        .filter(|(r, _)| r.area() > 0)
                        .collect();
                    for (i, (r, t)) in zones.iter().enumerate() {
                        assert!(
                            r.right() <= w && r.bottom() <= h,
                            "{t:?} at {r:?} is off a {w}x{h} screen"
                        );
                        for (r2, t2) in &zones[i + 1..] {
                            assert!(
                                !r.intersects(*r2),
                                "{t:?} at {r:?} overlaps {t2:?} at {r2:?} on {w}x{h}"
                            );
                        }
                    }
                }
                if !self.checks.view {
                    return;
                }
                let a = &self.app;
                let list = a.hits.list;
                if list.height > 0 {
                    let (rows, n) = (list.height as usize, a.stories.len());
                    if n >= rows {
                        assert!(
                            a.offset + rows <= n,
                            "rows {}..{} of {n}",
                            a.offset,
                            a.offset + rows
                        );
                    } else {
                        assert_eq!(a.offset, 0, "{n} stories in {rows} rows");
                    }
                    if a.follow && n > 0 {
                        assert!(
                            (a.offset..a.offset + rows).contains(&a.selected),
                            "cursor {} out of view at {}+{rows}",
                            a.selected,
                            a.offset
                        );
                    }
                }
                let d = a.hits.detail;
                if a.detail_scroll > 0
                    && a.selected_story().is_some()
                    && d.width >= 12
                    && d.height >= 2
                {
                    let lines = self.document_lines();
                    assert!(
                        self.app.detail_scroll <= lines.saturating_sub(d.height as usize),
                        "scrolled to {} of a {lines}-line document in {} rows",
                        self.app.detail_scroll,
                        d.height
                    );
                }
            }

            /// How long the comments pane's document is at this width, found
            /// by drawing it unscrolled on a screen tall enough to hold it all.
            /// The pane is as wide at any height.
            fn document_lines(&mut self) -> usize {
                let a = &mut self.app;
                let kept = (a.offset, a.detail_scroll, a.show_help);
                (a.detail_scroll, a.show_help) = (0, false);
                let size = self.size;
                let mut tall = 64;
                let lines = loop {
                    self.size = (size.0, tall);
                    let buf = self.draw();
                    let d = self.app.hits.detail;
                    let last = (d.y..d.bottom())
                        .rev()
                        .find(|&y| (d.x..d.right()).any(|x| buf[(x, y)].symbol() != " "));
                    // A run of blank rows at the bottom longer than any gap
                    // inside a document (spacers, paragraph breaks): all of
                    // it is on screen.
                    if last.is_none_or(|y| y + 8 < d.bottom()) || tall >= 8192 {
                        break last.map_or(0, |y| (y - d.y + 1) as usize);
                    }
                    tall *= 2;
                };
                self.size = size;
                let a = &mut self.app;
                (a.offset, a.detail_scroll, a.show_help) = kept;
                self.draw();
                lines
            }
        }

        fn run(start: (u16, u16), server: Server, ops: &[Op], checks: Checks) {
            let mut s = Session::new(start, server, checks);
            for op in ops {
                s.apply(op);
            }
            s.finish();
        }

        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: 64,
                max_shrink_iters: 512,
                // A failure is reported with its shrunk case; nothing is
                // written into the source tree.
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(config())]

            #[test]
            fn random_sessions_never_panic_and_keep_the_view_consistent(
                start in size(),
                server in server(),
                ops in prop::collection::vec(op(), 1..80),
            ) {
                run(start, server, &ops, Checks { view: true, ..Checks::default() });
            }

            #[test]
            fn random_sessions_put_every_click_target_on_screen_and_none_on_another(
                start in size(),
                server in server(),
                ops in prop::collection::vec(op(), 1..80),
            ) {
                run(start, server, &ops, Checks { zones: true, ..Checks::default() });
            }

            #[test]
            fn random_sessions_never_skip_a_page_of_the_feed(
                start in size(),
                server in server(),
                ops in prop::collection::vec(op(), 1..80),
            ) {
                run(start, server, &ops, Checks { pages: true, ..Checks::default() });
            }
        }
    }
}
