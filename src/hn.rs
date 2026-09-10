//! Hacker News over HTTP: the Firebase API for feeds and items, Algolia for
//! search and for a whole comment tree in one request.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::util::clean;

const FIREBASE: &str = "https://hacker-news.firebaseio.com/v0";
const ALGOLIA: &str = "https://hn.algolia.com/api/v1";

/// Where requests go instead, for the end-to-end tests' stand-in server.
const FIREBASE_ENV: &str = "HACKNEY_FIREBASE_URL";
const ALGOLIA_ENV: &str = "HACKNEY_ALGOLIA_URL";

const TIMEOUT: Duration = Duration::from_secs(15);

/// Parallel requests when fetching a page of items.
const WORKERS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feed {
    Top,
    New,
    Best,
    Ask,
    Show,
    Jobs,
    Search,
}

impl Feed {
    pub const TABS: [Feed; 6] = [
        Feed::Top,
        Feed::New,
        Feed::Best,
        Feed::Ask,
        Feed::Show,
        Feed::Jobs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Feed::Top => "Top",
            Feed::New => "New",
            Feed::Best => "Best",
            Feed::Ask => "Ask",
            Feed::Show => "Show",
            Feed::Jobs => "Jobs",
            Feed::Search => "Search",
        }
    }

    /// What the nav line calls the list.
    pub fn heading(self) -> &'static str {
        match self {
            Feed::Top => "Top stories",
            Feed::New => "New stories",
            Feed::Best => "Best stories",
            Feed::Ask => "Ask HN",
            Feed::Show => "Show HN",
            Feed::Jobs => "Jobs",
            Feed::Search => "Search",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Feed::Top => "topstories",
            Feed::New => "newstories",
            Feed::Best => "beststories",
            Feed::Ask => "askstories",
            Feed::Show => "showstories",
            Feed::Jobs => "jobstories",
            Feed::Search => unreachable!("search has no feed endpoint"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Story {
    pub id: u64,
    pub title: String,
    pub by: String,
    pub score: u32,
    pub time: i64,
    pub comments: u32,
    pub url: Option<String>,
    pub text: Option<String>,
    pub kids: Vec<u64>,
}

impl Story {
    pub fn hn_url(&self) -> String {
        format!("https://news.ycombinator.com/item?id={}", self.id)
    }

    /// The article, or the discussion for text posts.
    pub fn link(&self) -> String {
        self.url.clone().unwrap_or_else(|| self.hn_url())
    }

    pub fn domain(&self) -> Option<&str> {
        let rest = self.url.as_deref()?.split_once("://")?.1;
        let host = rest.split(['/', '?', '#']).next()?;
        // Credentials in front are not the host: `news.ycombinator.com@evil`
        // must read as `evil`.
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        Some(host.strip_prefix("www.").unwrap_or(host))
    }
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub by: String,
    pub time: i64,
    /// HN's HTML, as served.
    pub text: String,
    pub depth: usize,
    pub children: Vec<Comment>,
}

/// A Firebase item. Every field is optional there, and a missing item is `null`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Item {
    id: u64,
    by: Option<String>,
    time: i64,
    title: Option<String>,
    url: Option<String>,
    text: Option<String>,
    score: u32,
    descendants: u32,
    kids: Vec<u64>,
    deleted: bool,
    dead: bool,
}

impl From<Item> for Story {
    fn from(i: Item) -> Self {
        Story {
            id: i.id,
            title: clean(i.title.unwrap_or_default()),
            by: clean(i.by.unwrap_or_default()),
            score: i.score,
            time: i.time,
            comments: i.descendants,
            url: i.url.filter(|u| !u.is_empty()),
            text: i.text.filter(|t| !t.is_empty()),
            kids: i.kids,
        }
    }
}

/// A node in Algolia's comment tree.
#[derive(Debug, Deserialize)]
struct Node {
    id: u64,
    author: Option<String>,
    text: Option<String>,
    #[serde(default)]
    created_at_i: i64,
    #[serde(default)]
    children: Vec<Node>,
}

#[derive(Debug, Deserialize)]
struct Hits {
    /// Each read on its own, so one malformed hit drops out rather than
    /// failing the whole search.
    hits: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Hit {
    #[serde(rename = "objectID")]
    object_id: String,
    title: Option<String>,
    url: Option<String>,
    author: Option<String>,
    points: Option<u32>,
    num_comments: Option<u32>,
    #[serde(default)]
    created_at_i: i64,
    story_text: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    firebase: String,
    algolia: String,
}

impl Client {
    /// The real APIs, unless the environment points elsewhere.
    pub fn new() -> Self {
        let base = |var, default: &str| std::env::var(var).unwrap_or_else(|_| default.into());
        Self::at(
            &base(FIREBASE_ENV, FIREBASE),
            &base(ALGOLIA_ENV, ALGOLIA),
            TIMEOUT,
        )
    }

    /// Firebase and Algolia at the given base URLs.
    pub fn at(firebase: &str, algolia: &str, timeout: Duration) -> Self {
        // Keep a warm connection for every worker on both hosts: the default
        // pool of three per host meant a fresh TLS handshake for most requests.
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent("hackney")
            .max_idle_connections(4 * WORKERS)
            .max_idle_connections_per_host(2 * WORKERS)
            .build()
            .new_agent();
        Self {
            agent,
            firebase: firebase.trim_end_matches('/').into(),
            algolia: algolia.trim_end_matches('/').into(),
        }
    }

    fn get<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)]) -> Result<T> {
        let mut req = self.agent.get(url);
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        let text = req
            .call()
            .with_context(|| format!("GET {url}"))?
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_to_string()
            .with_context(|| format!("GET {url}: reading the response"))?;
        // Comment trees nest deeper than serde_json's default limit allows,
        // so the limit is off; this bound stands in for it. Parsing recurses
        // once per level, and a stack overflow aborts rather than unwinds.
        anyhow::ensure!(
            nesting(&text) <= MAX_NESTING,
            "GET {url}: nested more than {MAX_NESTING} deep"
        );
        let parse = || -> Result<T> {
            let mut de = serde_json::Deserializer::from_str(&text);
            de.disable_recursion_limit();
            let value = T::deserialize(&mut de)?;
            de.end()?;
            Ok(value)
        };
        parse().with_context(|| format!("GET {url}: not the JSON expected"))
    }

    pub fn feed(&self, feed: Feed) -> Result<Vec<u64>> {
        self.get(&format!("{}/{}.json", self.firebase, feed.endpoint()), &[])
    }

    fn item(&self, id: u64) -> Result<Option<Item>> {
        self.get(&format!("{}/item/{id}.json", self.firebase), &[])
    }

    /// Items in the order asked for. A page with a few failed requests still
    /// shows the rest; only a page where every request failed is an error.
    fn items(&self, ids: &[u64]) -> Result<Vec<Item>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let per = ids.len().div_ceil(WORKERS);
        let results: Vec<Result<Option<Item>>> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .chunks(per)
                .map(|chunk| {
                    s.spawn(move || chunk.iter().map(|&id| self.item(id)).collect::<Vec<_>>())
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });

        let mut items = Vec::new();
        let mut any_ok = false;
        let mut first_err = None;
        for r in results {
            match r {
                Ok(item) => {
                    any_ok = true;
                    items.extend(item);
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) if !any_ok => Err(e),
            _ => Ok(items),
        }
    }

    pub fn stories(&self, ids: &[u64]) -> Result<Vec<Story>> {
        Ok(self
            .items(ids)?
            .into_iter()
            .filter(|i| !i.deleted && !i.dead)
            .map(Story::from)
            .collect())
    }

    pub fn search(&self, query: &str) -> Result<Vec<Story>> {
        let hits: Hits = self.get(
            &format!("{}/search", self.algolia),
            &[("query", query), ("tags", "story"), ("hitsPerPage", "50")],
        )?;
        Ok(hits
            .hits
            .into_iter()
            .filter_map(|h| {
                let h: Hit = serde_json::from_value(h).ok()?;
                Some(Story {
                    id: h.object_id.parse().ok()?,
                    title: clean(h.title.unwrap_or_default()),
                    by: clean(h.author.unwrap_or_default()),
                    score: h.points.unwrap_or_default(),
                    time: h.created_at_i,
                    comments: h.num_comments.unwrap_or_default(),
                    url: h.url.filter(|u| !u.is_empty()),
                    text: h.story_text.filter(|t| !t.is_empty()),
                    kids: Vec::new(),
                })
            })
            .collect())
    }

    /// The story, fresh, and its comments with the top level in HN's own
    /// ranking (Algolia returns them in posting order). The two requests run
    /// side by side.
    pub fn thread(&self, id: u64) -> Result<(Story, Vec<Comment>)> {
        // The tree is parsed on this thread, which has the deep stack for it.
        let (item, tree) = std::thread::scope(|s| {
            let item = s.spawn(|| self.item(id));
            let tree = self.get::<Node>(&format!("{}/items/{id}", self.algolia), &[]);
            (item.join(), tree)
        });
        let item = item.unwrap_or_else(|_| Err(anyhow::anyhow!("fetching story {id} panicked")))?;
        let story = Story::from(item.context("that story no longer exists")?);
        let comments = match tree {
            Ok(tree) => {
                let rank: HashMap<u64, usize> = story
                    .kids
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (*k, i))
                    .collect();
                let mut top = tree.children;
                top.sort_by_key(|n| rank.get(&n.id).copied().unwrap_or(usize::MAX));
                top.into_iter().filter_map(|n| from_node(n, 0)).collect()
            }
            // Algolia runs a minute or two behind brand-new stories.
            Err(_) => self.firebase_tree(&story.kids),
        };
        Ok((story, comments))
    }

    /// The tree from Firebase, one round of parallel requests per level of
    /// nesting rather than one per comment with replies.
    fn firebase_tree(&self, top: &[u64]) -> Vec<Comment> {
        let mut items = HashMap::new();
        let mut seen: HashSet<u64> = top.iter().copied().collect();
        let mut level = top.to_vec();
        while !level.is_empty() {
            let fetched = self.items(&level).unwrap_or_default();
            level = fetched
                .iter()
                .filter(|i| !i.dead)
                .flat_map(|i| i.kids.iter().copied())
                .filter(|k| seen.insert(*k))
                .collect();
            items.extend(fetched.into_iter().map(|i| (i.id, i)));
        }
        build(&mut items, top, 0)
    }
}

/// Firebase items into a tree, in HN's order. Dead comments go, with their
/// replies; deleted ones stay only as a placeholder for replies.
fn build(items: &mut HashMap<u64, Item>, ids: &[u64], depth: usize) -> Vec<Comment> {
    ids.iter()
        .filter_map(|id| {
            let i = items.remove(id).filter(|i| !i.dead)?;
            let children = build(items, &i.kids, depth + 1);
            (!i.deleted || !children.is_empty()).then(|| Comment {
                id: i.id,
                by: clean(i.by.unwrap_or_else(|| "[deleted]".into())),
                time: i.time,
                text: i.text.unwrap_or_else(|| "<i>[deleted]</i>".into()),
                depth,
                children,
            })
        })
        .collect()
}

/// Deepest JSON accepted: 20,000 levels, two per comment (its object and its
/// `children`), so replies 10,000 deep. Real threads are a few hundred deep;
/// a worker's 64 MiB stack parses about 14,000 in a debug build and 97,000
/// in release.
const MAX_NESTING: usize = 20_000;

/// How deeply `json` nests arrays and objects, strings skipped.
fn nesting(json: &str) -> usize {
    let (mut depth, mut max, mut in_str, mut escaped) = (0usize, 0, false, false);
    for b in json.bytes() {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Deleted comments with no replies drop out; with replies they stay, as a
/// placeholder the replies can hang from.
fn from_node(n: Node, depth: usize) -> Option<Comment> {
    let children: Vec<Comment> = n
        .children
        .into_iter()
        .filter_map(|c| from_node(c, depth + 1))
        .collect();
    let text = n.text.filter(|t| !t.is_empty());
    if text.is_none() && children.is_empty() {
        return None;
    }
    Some(Comment {
        id: n.id,
        by: clean(n.author.unwrap_or_else(|| "[deleted]".into())),
        time: n.created_at_i,
        text: text.unwrap_or_else(|| "<i>[deleted]</i>".into()),
        depth,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_hn::{FakeHn, node};
    use serde_json::{Value, json};
    use std::fmt::Write as _;
    use std::time::Instant;

    /// As `app::WORKER_STACK`: the stack a thread is fetched and parsed on.
    const WORKER_STACK: usize = 64 << 20;

    fn client(hn: &FakeHn) -> Client {
        Client::at(&hn.firebase(), &hn.algolia(), Duration::from_secs(5))
    }

    fn impatient(hn: &FakeHn, timeout: Duration) -> Client {
        Client::at(&hn.firebase(), &hn.algolia(), timeout)
    }

    /// The whole error chain, as the app shows it.
    fn message(e: anyhow::Error) -> String {
        format!("{e:#}")
    }

    fn ids(stories: &[Story]) -> Vec<u64> {
        stories.iter().map(|s| s.id).collect()
    }

    fn at_url(url: &str) -> Story {
        Story {
            url: Some(url.into()),
            ..Story::default()
        }
    }

    /// Every comment with the depth it actually sits at, in reading order,
    /// checking each one's own `depth` agrees. Iterative, so any depth will do.
    fn walk(comments: &[Comment]) -> Vec<(&Comment, usize)> {
        let mut out = Vec::new();
        let mut stack: Vec<(&Comment, usize)> = comments.iter().rev().map(|c| (c, 0)).collect();
        while let Some((c, d)) = stack.pop() {
            assert_eq!(c.depth, d, "comment {} claims the wrong depth", c.id);
            out.push((c, d));
            stack.extend(c.children.iter().rev().map(|k| (k, d + 1)));
        }
        out
    }

    /// The tree's shape: (id, depth) in reading order.
    fn outline(comments: &[Comment]) -> Vec<(u64, usize)> {
        walk(comments).into_iter().map(|(c, d)| (c.id, d)).collect()
    }

    fn find(comments: &[Comment], id: u64) -> &Comment {
        walk(comments)
            .into_iter()
            .map(|(c, _)| c)
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no comment {id}"))
    }

    /// How long a chain of single replies is, checking it never branches.
    fn chain_len(comments: &[Comment]) -> usize {
        let mut n = 0;
        let mut level = comments;
        while let [only] = level {
            n += 1;
            level = &only.children;
        }
        assert!(level.is_empty(), "the chain branches after {n}");
        n
    }

    /// Algolia's tree for `story` as one chain of replies `depth` long, ids
    /// from 1_000_000. Written as text: a `serde_json::Value` that deep would
    /// overflow this test's own stack building and dropping it.
    fn algolia_chain(story: u64, depth: usize) -> String {
        let mut s = format!(r#"{{"id":{story},"author":"octocat","children":["#);
        for i in 0..depth {
            let _ = write!(
                s,
                r#"{{"id":{},"author":"octocat","text":"reply","created_at_i":1,"children":["#,
                1_000_000 + i
            );
        }
        s.push_str(&"]}".repeat(depth + 1));
        s
    }

    /// A deleted comment as Firebase serves it: no author, no text.
    fn deleted_item(id: u64, kids: &[u64]) -> Value {
        json!({"id": id, "type": "comment", "deleted": true, "time": 1, "kids": kids})
    }

    fn dead_item(id: u64, kids: &[u64]) -> Value {
        json!({"id": id, "type": "comment", "dead": true, "by": "octocat",
               "text": "flagged", "time": 1, "kids": kids})
    }

    /// A deleted comment as Algolia serves it.
    fn deleted_node(id: u64, children: Value) -> Value {
        json!({"id": id, "author": null, "text": null, "created_at_i": 1, "children": children})
    }

    fn leaf(id: u64) -> Value {
        node(id, "octocat", &format!("comment {id}"), json!([]))
    }

    /// Run `f` on a thread with `stack` bytes of stack, as the app's workers
    /// run a fetch. A panic inside comes out here with its own message.
    fn on_stack<T: Send + 'static>(stack: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    /// Wait up to `limit` for `done`, polling the fake's log.
    fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while !done() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// A logged request's query string, decoded into sorted pairs.
    fn query_pairs(request: &str) -> Vec<(String, String)> {
        fn decode(s: &str) -> String {
            let mut out = Vec::new();
            let mut bytes = s.bytes();
            while let Some(b) = bytes.next() {
                out.push(match b {
                    b'+' => b' ',
                    b'%' => {
                        let hex: String = bytes.by_ref().take(2).map(char::from).collect();
                        u8::from_str_radix(&hex, 16).expect("a percent escape")
                    }
                    b => b,
                });
            }
            String::from_utf8(out).expect("UTF-8")
        }
        let query = request.split_once('?').map_or("", |(_, q)| q);
        let mut pairs: Vec<_> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        pairs.sort();
        pairs
    }

    #[test]
    fn domain_drops_scheme_path_and_www() {
        let s = |url: &str| Story {
            url: Some(url.into()),
            ..Story::default()
        };
        assert_eq!(
            s("https://www.example.com/a/b?c").domain(),
            Some("example.com")
        );
        assert_eq!(
            s("http://blog.rust-lang.org").domain(),
            Some("blog.rust-lang.org")
        );
        assert_eq!(Story::default().domain(), None);
    }

    #[test]
    fn deleted_leaves_drop_but_deleted_parents_stay() {
        let tree: Node = serde_json::from_str(
            r#"{"id":1,"author":null,"text":null,"children":[
                {"id":2,"author":"a","text":"hi","created_at_i":5,"children":[]},
                {"id":3,"author":null,"text":null,"children":[]}
            ]}"#,
        )
        .unwrap();
        let c = from_node(tree, 0).expect("kept for its reply");
        assert_eq!(c.by, "[deleted]");
        assert_eq!(c.children.len(), 1);
        assert_eq!(c.children[0].depth, 1);
    }

    #[test]
    fn a_page_of_stories_comes_back_in_order_from_the_fake() {
        let hn = crate::fake_hn::FakeHn::start();
        hn.feed("topstories", &[3, 1, 2]);
        for id in [1, 2, 3] {
            hn.story(id, &format!("story {id}"), &[]);
        }
        let c = Client::at(&hn.firebase(), &hn.algolia(), Duration::from_secs(5));
        let ids = c.feed(Feed::Top).unwrap();
        let titles: Vec<String> = c
            .stories(&ids)
            .unwrap()
            .into_iter()
            .map(|s| s.title)
            .collect();
        assert_eq!(titles, ["story 3", "story 1", "story 2"]);
    }

    // ----------------------------------------------------------------- feeds

    #[test]
    fn every_tab_reads_its_own_firebase_feed_in_the_order_served() {
        let hn = FakeHn::start();
        let tabs = [
            (Feed::Top, "topstories"),
            (Feed::New, "newstories"),
            (Feed::Best, "beststories"),
            (Feed::Ask, "askstories"),
            (Feed::Show, "showstories"),
            (Feed::Jobs, "jobstories"),
        ];
        for (n, (_, name)) in tabs.iter().enumerate() {
            hn.feed(name, &[30, n as u64, 10]);
        }
        let c = client(&hn);
        for (n, (feed, _)) in tabs.iter().enumerate() {
            assert_eq!(c.feed(*feed).unwrap(), [30, n as u64, 10], "{feed:?}");
        }
        let asked: Vec<String> = tabs
            .iter()
            .map(|(_, name)| format!("/v0/{name}.json"))
            .collect();
        assert_eq!(hn.requests(), asked);
    }

    #[test]
    fn an_empty_feed_is_an_empty_list() {
        let hn = FakeHn::start();
        hn.feed("jobstories", &[]);
        assert_eq!(client(&hn).feed(Feed::Jobs).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn a_feed_that_fails_is_an_error_naming_its_url_and_status() {
        let hn = FakeHn::start();
        hn.route("/v0/topstories.json", 500, "oops");
        let c = client(&hn);
        let e = message(c.feed(Feed::Top).unwrap_err());
        assert!(
            e.contains(&format!("GET {}/topstories.json", hn.firebase())),
            "{e}"
        );
        assert!(e.contains("500"), "{e}");

        // Not routed: the fake's 404.
        let e = message(c.feed(Feed::Best).unwrap_err());
        assert!(
            e.contains(&format!("GET {}/beststories.json", hn.firebase())),
            "{e}"
        );
        assert!(e.contains("404"), "{e}");
    }

    #[test]
    fn a_feed_that_is_not_a_list_of_ids_is_an_error() {
        let hn = FakeHn::start();
        let c = client(&hn);
        for body in [
            "",
            "not json",
            "<html>rate limited</html>",
            "null",
            r#"{"ids":[1]}"#,
            r#"[1,"2"]"#,
            "[1,-2]",
            "[1.5]",
            "[1,2",
            "[1,2] [3]",
            "[1,2]x",
        ] {
            hn.route("/v0/topstories.json", 200, body);
            assert!(c.feed(Feed::Top).is_err(), "{body:?} parsed");
        }
        // Whitespace around the JSON is not trailing garbage.
        hn.route("/v0/topstories.json", 200, " \n[1, 2]\r\n ");
        assert_eq!(c.feed(Feed::Top).unwrap(), [1, 2]);
    }

    #[test]
    fn trailing_slashes_on_the_base_urls_are_ignored() {
        let hn = FakeHn::start();
        hn.feed("topstories", &[1]);
        hn.search(json!([]));
        let c = Client::at(
            &format!("{}/", hn.firebase()),
            &format!("{}//", hn.algolia()),
            Duration::from_secs(5),
        );
        c.feed(Feed::Top).unwrap();
        c.search("rust").unwrap();
        assert_eq!(hn.hits("/v0/topstories.json"), 1);
        assert_eq!(hn.hits("/algolia/v1/search"), 1);
    }

    #[test]
    fn a_server_that_hangs_times_out_promptly_naming_the_url() {
        let hn = FakeHn::start();
        hn.feed("topstories", &[1]);
        hn.delay("/v0/topstories.json", Duration::from_secs(10));
        let start = Instant::now();
        let e = message(
            impatient(&hn, Duration::from_millis(300))
                .feed(Feed::Top)
                .unwrap_err(),
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
        assert!(
            e.contains(&format!("GET {}/topstories.json", hn.firebase())),
            "{e}"
        );
    }

    #[test]
    fn a_response_over_ureqs_default_ten_mib_limit_still_loads() {
        // The biggest threads' trees run to megabytes.
        let hn = FakeHn::start();
        hn.story(1, "big", &[2]);
        let text = "x".repeat(12 << 20);
        hn.tree(1, json!([node(2, "octocat", &text, json!([]))]));
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(hn.hits("/v0/item/2.json"), 0, "fell back to Firebase");
        assert_eq!(comments[0].text.len(), 12 << 20);
    }

    // --------------------------------------------------------------- stories

    #[test]
    fn a_page_keeps_the_order_asked_for_when_early_ids_answer_last() {
        let hn = FakeHn::start();
        for id in 1..=5 {
            hn.story(id, &format!("story {id}"), &[]);
            hn.delay(
                &format!("/v0/item/{id}.json"),
                Duration::from_millis(100 * (6 - id)),
            );
        }
        let page = client(&hn).stories(&[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(ids(&page), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_page_larger_than_the_worker_pool_keeps_its_order() {
        let hn = FakeHn::start();
        // A permutation of 1..=40: out of numeric order, and more ids than
        // workers, so each worker takes a run of them.
        let asked: Vec<u64> = (1..=40).map(|i| i * 7 % 41).collect();
        for &id in &asked {
            hn.story(id, &format!("story {id}"), &[]);
        }
        assert_eq!(ids(&client(&hn).stories(&asked).unwrap()), asked);
    }

    #[test]
    fn a_page_with_some_failed_items_still_shows_the_rest() {
        let hn = FakeHn::start();
        hn.story(1, "fine", &[]);
        hn.route("/v0/item/2.json", 500, "oops");
        hn.route("/v0/item/3.json", 200, "<html>rate limited</html>");
        hn.route("/v0/item/4.json", 200, r#"{"id":4,"score":"lots"}"#);
        // 5 is not routed: a 404.
        hn.route("/v0/item/6.json", 200, r#"{"id":6,"title":"cut sh"#);
        hn.story(7, "also fine", &[]);
        let page = client(&hn).stories(&[1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert_eq!(ids(&page), [1, 7]);
    }

    #[test]
    fn a_page_where_every_item_fails_is_an_error_naming_the_first() {
        let hn = FakeHn::start();
        hn.route("/v0/item/1.json", 500, "oops");
        hn.route("/v0/item/2.json", 200, "garbage");
        let e = message(client(&hn).stories(&[1, 2, 3]).unwrap_err());
        assert!(
            e.contains(&format!("GET {}/item/1.json", hn.firebase())),
            "{e}"
        );
    }

    #[test]
    fn missing_items_drop_out_of_a_page() {
        let hn = FakeHn::start();
        hn.story(1, "one", &[]);
        hn.route("/v0/item/2.json", 200, "null");
        hn.story(3, "three", &[]);
        assert_eq!(ids(&client(&hn).stories(&[1, 2, 3]).unwrap()), [1, 3]);
    }

    #[test]
    fn a_missing_item_is_an_answer_so_a_page_of_them_is_empty_not_an_error() {
        let hn = FakeHn::start();
        hn.route("/v0/item/1.json", 200, "null");
        hn.route("/v0/item/2.json", 200, "null");
        hn.route("/v0/item/3.json", 500, "oops");
        assert!(client(&hn).stories(&[1, 2, 3]).unwrap().is_empty());
    }

    #[test]
    fn dead_and_deleted_stories_are_left_out() {
        let hn = FakeHn::start();
        hn.story(1, "one", &[]);
        hn.item(json!({"id": 2, "type": "story", "dead": true, "title": "flagged"}));
        hn.item(json!({"id": 3, "type": "story", "deleted": true}));
        hn.item(json!({"id": 4, "dead": true, "deleted": true}));
        hn.story(5, "five", &[]);
        hn.item(json!({"id": 6, "title": "six", "dead": false, "deleted": false}));
        let page = client(&hn).stories(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(ids(&page), [1, 5, 6]);
    }

    #[test]
    fn blank_urls_and_texts_become_none_and_the_link_falls_back_to_the_discussion() {
        let hn = FakeHn::start();
        hn.item(json!({"id": 1, "title": "Ask HN: blank", "url": "", "text": ""}));
        hn.item(json!({"id": 2, "title": "Show HN: both",
                       "url": "https://example.com/x", "text": "<p>Hi &amp; bye"}));
        let page = client(&hn).stories(&[1, 2]).unwrap();
        assert_eq!(
            (page[0].url.as_deref(), page[0].text.as_deref()),
            (None, None)
        );
        assert_eq!(page[0].link(), "https://news.ycombinator.com/item?id=1");
        assert_eq!(page[1].url.as_deref(), Some("https://example.com/x"));
        assert_eq!(page[1].text.as_deref(), Some("<p>Hi &amp; bye"));
        assert_eq!(page[1].link(), "https://example.com/x");
    }

    #[test]
    fn control_characters_are_stripped_from_titles_and_authors() {
        let hn = FakeHn::start();
        hn.item(json!({"id": 1, "title": "Tab\there\u{1b}[31m red\u{7}\r\n",
                       "by": "octo\u{0}cat\u{85}"}));
        hn.item(json!({"id": 2, "title": "Café — 日本語 🦀", "by": "octocat"}));
        let page = client(&hn).stories(&[1, 2]).unwrap();
        assert_eq!(page[0].title, "Tabhere[31m red");
        assert_eq!(page[0].by, "octocat");
        assert_eq!(page[1].title, "Café — 日本語 🦀");
    }

    #[test]
    fn an_item_with_nothing_but_an_id_is_a_story_of_defaults() {
        let hn = FakeHn::start();
        hn.route("/v0/item/7.json", 200, r#"{"id":7}"#);
        let page = client(&hn).stories(&[7]).unwrap();
        let s = &page[0];
        assert_eq!(
            (
                s.id,
                s.title.as_str(),
                s.by.as_str(),
                s.score,
                s.time,
                s.comments
            ),
            (7, "", "", 0, 0, 0)
        );
        assert!(s.url.is_none() && s.text.is_none() && s.kids.is_empty());
    }

    #[test]
    fn a_story_carries_its_firebase_fields_across_and_ignores_the_rest() {
        let hn = FakeHn::start();
        hn.item(json!({
            "id": 42, "type": "story", "by": "octocat", "title": "Hello",
            "url": "https://example.com/", "score": 321, "time": 1_700_000_000,
            "descendants": 17, "kids": [9, 8, 7],
            "parts": [1, 2], "poll": 3, "future_field": {"nested": [[{}]]},
        }));
        let page = client(&hn).stories(&[42]).unwrap();
        let s = &page[0];
        assert_eq!(
            (s.id, s.title.as_str(), s.by.as_str(), s.score, s.time),
            (42, "Hello", "octocat", 321, 1_700_000_000)
        );
        // The count of every comment below, not just the top level's.
        assert_eq!(s.comments, 17);
        assert_eq!(s.kids, [9, 8, 7]);
        assert_eq!(s.url.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn a_page_of_no_ids_asks_nothing() {
        let hn = FakeHn::start();
        assert!(client(&hn).stories(&[]).unwrap().is_empty());
        assert!(hn.requests().is_empty());
    }

    #[test]
    fn a_page_is_fetched_in_parallel() {
        let hn = FakeHn::start();
        let page: Vec<u64> = (1..=32).collect();
        for &id in &page {
            hn.story(id, "slow", &[]);
            hn.delay(&format!("/v0/item/{id}.json"), Duration::from_millis(250));
        }
        let start = Instant::now();
        let got = client(&hn).stories(&page).unwrap();
        let took = start.elapsed();
        assert_eq!(got.len(), 32);
        // One at a time is 8s; sixteen workers take about half a second.
        assert!(took < Duration::from_secs(4), "took {took:?}");
    }

    #[test]
    fn one_hung_item_times_out_without_sinking_the_page() {
        let hn = FakeHn::start();
        for id in 1..=3 {
            hn.story(id, "story", &[]);
        }
        hn.delay("/v0/item/2.json", Duration::from_secs(10));
        let start = Instant::now();
        let page = impatient(&hn, Duration::from_millis(500))
            .stories(&[1, 2, 3])
            .unwrap();
        assert_eq!(ids(&page), [1, 3]);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
    }

    // ---------------------------------------------------------------- search

    #[test]
    fn search_sends_the_query_verbatim_with_story_tags_and_fifty_per_page() {
        let hn = FakeHn::start();
        hn.search(json!([]));
        // Everything a query string could be confused by, including an
        // attempt to smuggle in a parameter of its own.
        let q = "rust & go?tags=comment#frag 100% +1 /é";
        client(&hn).search(q).unwrap();
        let log = hn.requests();
        assert_eq!(log.len(), 1);
        assert!(log[0].starts_with("/algolia/v1/search?"), "{}", log[0]);
        let want = [("hitsPerPage", "50"), ("query", q), ("tags", "story")]
            .map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(query_pairs(&log[0]), want, "{}", log[0]);
    }

    #[test]
    fn search_hits_become_stories_in_the_order_served() {
        let hn = FakeHn::start();
        hn.search(json!([
            {"objectID": "42", "title": "Hello", "url": "https://example.com/",
             "author": "octocat", "points": 321, "num_comments": 17,
             "created_at_i": 1_700_000_000, "story_text": null,
             "_highlightResult": {"title": {"value": "<em>Hello</em>"}}},
            {"objectID": "7", "title": "Ask HN: why?", "url": null, "author": "octocat",
             "points": 5, "num_comments": 0, "created_at_i": 1_600_000_000,
             "story_text": "<p>Because"},
        ]));
        let found = client(&hn).search("hello").unwrap();
        assert_eq!(ids(&found), [42, 7]);
        let s = &found[0];
        assert_eq!(
            (s.title.as_str(), s.by.as_str(), s.score, s.comments, s.time),
            ("Hello", "octocat", 321, 17, 1_700_000_000)
        );
        assert_eq!(s.url.as_deref(), Some("https://example.com/"));
        assert_eq!(s.text, None);
        assert!(s.kids.is_empty());
        assert_eq!(found[1].url, None);
        assert_eq!(found[1].text.as_deref(), Some("<p>Because"));
    }

    #[test]
    fn search_skips_hits_without_a_numeric_id() {
        let hn = FakeHn::start();
        let hit = |id: &str| json!({"objectID": id, "title": id});
        hn.search(json!([
            hit("abc"),
            hit(""),
            hit("12a"),
            hit("-5"),
            hit(" 9"),
            hit("1e3"),
            hit("18446744073709551616"),
            hit("3"),
        ]));
        assert_eq!(ids(&client(&hn).search("x").unwrap()), [3]);
    }

    #[test]
    fn search_hits_missing_or_null_fields_get_defaults() {
        let hn = FakeHn::start();
        hn.search(json!([
            {"objectID": "1"},
            {"objectID": "2", "title": null, "author": null, "points": null,
             "num_comments": null, "url": null, "story_text": null},
        ]));
        let found = client(&hn).search("x").unwrap();
        assert_eq!(ids(&found), [1, 2]);
        for s in &found {
            assert_eq!(
                (s.title.as_str(), s.by.as_str(), s.score, s.comments, s.time),
                ("", "", 0, 0, 0)
            );
            assert!(s.url.is_none() && s.text.is_none());
        }
    }

    #[test]
    fn search_drops_blank_urls_and_texts_and_strips_control_characters() {
        let hn = FakeHn::start();
        hn.search(json!([
            {"objectID": "1", "title": "a\u{1b}b\r\n", "author": "octo\tcat",
             "url": "", "story_text": ""},
        ]));
        let found = client(&hn).search("x").unwrap();
        let s = &found[0];
        assert_eq!((s.title.as_str(), s.by.as_str()), ("ab", "octocat"));
        assert_eq!((s.url.as_deref(), s.text.as_deref()), (None, None));
    }

    #[test]
    fn a_search_that_fails_or_is_not_hits_is_an_error_but_no_hits_is_not() {
        let hn = FakeHn::start();
        let c = client(&hn);
        hn.route("/algolia/v1/search", 500, "oops");
        let e = message(c.search("x").unwrap_err());
        assert!(e.contains(&format!("GET {}/search", hn.algolia())), "{e}");
        assert!(e.contains("500"), "{e}");
        for body in ["<html>", r#"{"nbHits":0}"#, r#"{"hits":null}"#, "[]"] {
            hn.route("/algolia/v1/search", 200, body);
            assert!(c.search("x").is_err(), "{body:?} parsed");
        }
        hn.search(json!([]));
        assert!(c.search("x").unwrap().is_empty());
    }

    // ------------------------------------------------- thread, from Algolia

    #[test]
    fn a_thread_is_one_firebase_request_and_one_algolia_request() {
        let hn = FakeHn::start();
        hn.story(1, "Fresh title", &[11, 12]);
        // Routed, so a needless per-comment fetch would succeed, not hide.
        hn.comment(11, "octocat", "from firebase", &[]);
        hn.comment(12, "octocat", "from firebase", &[]);
        hn.json(
            "/algolia/v1/items/1",
            &json!({"id": 1, "title": "Stale title", "points": 1, "children": [
                node(11, "octocat", "from algolia", json!([])),
                node(12, "octocat", "from algolia", json!([])),
            ]}),
        );
        let (story, comments) = client(&hn).thread(1).unwrap();
        // The story itself is Firebase's, fresh.
        assert_eq!((story.title.as_str(), story.score), ("Fresh title", 101));
        assert_eq!(outline(&comments), [(11, 0), (12, 0)]);
        assert!(comments.iter().all(|c| c.text == "from algolia"));
        let mut log = hn.requests();
        log.sort();
        assert_eq!(log, ["/algolia/v1/items/1", "/v0/item/1.json"]);
    }

    #[test]
    fn a_thread_asks_for_the_story_and_the_tree_at_once() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[]);
        hn.tree(1, json!([]));
        hn.delay("/v0/item/1.json", Duration::from_secs(3));
        hn.delay("/algolia/v1/items/1", Duration::from_secs(3));
        let c = client(&hn);
        let fetch = std::thread::spawn(move || c.thread(1));
        // One after the other, the second could not be asked for until the
        // first had been held its 3s.
        let both = wait_for(Duration::from_millis(2500), || hn.requests().len() == 2);
        assert!(both, "{:?}", hn.requests());
        fetch.join().unwrap().unwrap();
    }

    #[test]
    fn top_level_comments_follow_the_storys_ranking_with_unranked_ones_last() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[13, 11, 12]);
        // Algolia's posting order, with two comments the story's ranking
        // does not list yet.
        hn.tree(1, json!([leaf(11), leaf(14), leaf(12), leaf(13), leaf(10)]));
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(
            outline(&comments),
            [(13, 0), (11, 0), (12, 0), (14, 0), (10, 0)]
        );
    }

    #[test]
    fn replies_keep_algolias_order_and_count_their_depth() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12]);
        hn.tree(
            1,
            json!([
                node(
                    11,
                    "octocat",
                    "a",
                    json!([leaf(22), node(21, "octocat", "b", json!([leaf(31)])),])
                ),
                node(12, "octocat", "c", json!([leaf(23)])),
            ]),
        );
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(
            outline(&comments),
            [(11, 0), (22, 1), (21, 1), (31, 2), (12, 0), (23, 1)]
        );
    }

    #[test]
    fn deleted_algolia_comments_drop_unless_replies_hang_from_them() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12, 13, 14, 15]);
        hn.tree(
            1,
            json!([
                deleted_node(11, json!([leaf(21)])),
                deleted_node(12, json!([])),
                // Its only reply is deleted too, so nothing hangs from it.
                deleted_node(13, json!([deleted_node(22, json!([]))])),
                node(14, "octocat", "", json!([])),
                node(15, "octocat", "alive", json!([deleted_node(23, json!([]))])),
            ]),
        );
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(outline(&comments), [(11, 0), (21, 1), (15, 0)]);
        let placeholder = find(&comments, 11);
        assert_eq!(placeholder.by, "[deleted]");
        assert_eq!(placeholder.text, "<i>[deleted]</i>");
    }

    #[test]
    fn a_comment_keeps_its_html_and_time_but_not_control_characters_in_its_author() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11]);
        let html = "<p>a &amp; b<p><a href=\"https://example.com/\">link</a>\u{7}";
        hn.tree(
            1,
            json!([{"id": 11, "author": "octo\u{1b}cat\n", "text": html,
                    "created_at_i": 1_700_000_000, "children": []}]),
        );
        let (_, comments) = client(&hn).thread(1).unwrap();
        let c = &comments[0];
        assert_eq!(c.by, "octocat");
        assert_eq!(c.text, html);
        assert_eq!(c.time, 1_700_000_000);
    }

    #[test]
    fn a_story_firebase_no_longer_has_is_an_error() {
        let hn = FakeHn::start();
        hn.route("/v0/item/1.json", 200, "null");
        hn.tree(1, json!([leaf(11)]));
        let e = message(client(&hn).thread(1).unwrap_err());
        assert!(e.contains("no longer exists"), "{e}");
    }

    #[test]
    fn a_thread_whose_story_fails_to_load_is_an_error_naming_the_url() {
        let hn = FakeHn::start();
        hn.route("/v0/item/1.json", 500, "oops");
        hn.tree(1, json!([leaf(11)]));
        let e = message(client(&hn).thread(1).unwrap_err());
        assert!(
            e.contains(&format!("GET {}/item/1.json", hn.firebase())),
            "{e}"
        );
    }

    // ------------------------------------------------ thread, from Firebase

    #[test]
    fn when_algolia_fails_the_comments_come_from_firebase() {
        for (status, body) in [
            (404, "not found"),
            (500, "oops"),
            (200, "<html>"),
            (200, "null"),
            (200, r#"{"id":1,"children":["#),
            (200, r#"{"children":[]}"#),
            (200, r#"{"id":1,"children":null}"#),
        ] {
            let hn = FakeHn::start();
            hn.story(1, "story", &[11]);
            hn.comment(11, "octocat", "from firebase", &[]);
            hn.route("/algolia/v1/items/1", status, body);
            let (_, comments) = client(&hn).thread(1).unwrap();
            assert_eq!(outline(&comments), [(11, 0)], "{status} {body:?}");
            assert_eq!(comments[0].text, "from firebase");
        }
    }

    #[test]
    fn a_hung_algolia_falls_back_to_firebase_promptly() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11]);
        hn.comment(11, "octocat", "from firebase", &[]);
        hn.tree(1, json!([node(11, "octocat", "from algolia", json!([]))]));
        hn.delay("/algolia/v1/items/1", Duration::from_secs(10));
        let start = Instant::now();
        let (_, comments) = impatient(&hn, Duration::from_millis(500))
            .thread(1)
            .unwrap();
        assert_eq!(comments[0].text, "from firebase");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn the_firebase_tree_is_in_hns_order_at_every_level() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[3, 2]);
        hn.comment(3, "octocat", "a", &[5, 4]);
        hn.comment(2, "octocat", "b", &[6]);
        hn.comment(5, "octocat", "c", &[8, 7]);
        for id in [4, 6, 7, 8] {
            hn.comment(id, "octocat", "leaf", &[]);
        }
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(
            outline(&comments),
            [(3, 0), (5, 1), (8, 2), (7, 2), (4, 1), (2, 0), (6, 1)]
        );
    }

    #[test]
    fn the_firebase_tree_drops_dead_comments_and_never_asks_for_their_replies() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12, 13]);
        hn.comment(11, "octocat", "alive", &[]);
        hn.item(dead_item(12, &[21]));
        // Routed, so it would show if it were fetched.
        hn.comment(21, "octocat", "reply to the dead", &[]);
        hn.comment(13, "octocat", "alive", &[22, 23]);
        hn.item(dead_item(22, &[]));
        hn.comment(23, "octocat", "alive", &[]);
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(outline(&comments), [(11, 0), (13, 0), (23, 1)]);
        assert_eq!(hn.hits("/v0/item/21.json"), 0);
    }

    #[test]
    fn the_firebase_tree_keeps_deleted_comments_only_as_placeholders_for_replies() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12, 13, 14]);
        hn.item(deleted_item(11, &[21]));
        hn.comment(21, "octocat", "reply", &[]);
        hn.item(deleted_item(12, &[]));
        hn.item(deleted_item(13, &[22]));
        hn.item(dead_item(22, &[]));
        hn.item(deleted_item(14, &[23]));
        hn.item(deleted_item(23, &[]));
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(outline(&comments), [(11, 0), (21, 1)]);
        let placeholder = find(&comments, 11);
        assert_eq!(placeholder.by, "[deleted]");
        assert_eq!(placeholder.text, "<i>[deleted]</i>");
    }

    #[test]
    fn firebase_and_algolia_build_the_same_thread() {
        let fb = FakeHn::start();
        let al = FakeHn::start();
        for hn in [&fb, &al] {
            hn.story(1, "story", &[12, 11]);
        }
        let item = |id: u64, by: &str, kids: &[u64]| {
            json!({"id": id, "type": "comment", "by": by, "text": format!("<p>{id}"),
                   "time": 1_700_000_000 + id, "kids": kids})
        };
        let at = |id: u64, by: &str, children: Value| {
            json!({"id": id, "author": by, "text": format!("<p>{id}"),
                   "created_at_i": 1_700_000_000 + id, "children": children})
        };
        fb.item(item(12, "octocat", &[21, 22]));
        fb.item(deleted_item(21, &[31]));
        fb.item(item(31, "hubot", &[]));
        fb.item(item(22, "octocat\u{1b}", &[]));
        fb.item(item(11, "hubot", &[]));
        al.tree(
            1,
            json!([
                at(11, "hubot", json!([])),
                at(
                    12,
                    "octocat",
                    json!([
                        deleted_node(21, json!([at(31, "hubot", json!([]))])),
                        at(22, "octocat\u{1b}", json!([])),
                    ])
                ),
            ]),
        );
        let shown = |hn: &FakeHn| {
            let (_, comments) = client(hn).thread(1).unwrap();
            walk(&comments)
                .into_iter()
                .map(|(c, d)| format!("{} {d} {} {} {}", c.id, c.by, c.text, c.time))
                .collect::<Vec<_>>()
        };
        let from_firebase = shown(&fb);
        assert_eq!(fb.hits("/v0/item/31.json"), 1, "not the Firebase path");
        assert_eq!(from_firebase, shown(&al));
        assert_eq!(
            from_firebase,
            [
                "12 0 octocat <p>12 1700000012",
                "21 1 [deleted] <i>[deleted]</i> 1",
                "31 2 hubot <p>31 1700000031",
                "22 1 octocat <p>22 1700000022",
                "11 0 hubot <p>11 1700000011",
            ]
        );
    }

    #[test]
    fn the_firebase_tree_survives_replies_that_loop_back() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 13]);
        hn.comment(11, "octocat", "a", &[12]);
        hn.comment(12, "octocat", "b", &[11]);
        hn.comment(13, "octocat", "its own reply", &[13]);
        let c = client(&hn);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(c.thread(1).map(|(_, comments)| comments)));
        let comments = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("still going round the loop after 10s")
            .unwrap();
        assert_eq!(outline(&comments), [(11, 0), (12, 1), (13, 0)]);
        for id in [11, 12, 13] {
            assert_eq!(hn.hits(&format!("/v0/item/{id}.json")), 1, "item {id}");
        }
    }

    #[test]
    fn the_firebase_tree_asks_one_round_per_level_of_nesting() {
        let hn = FakeHn::start();
        let levels: [&[u64]; 4] = [&[10, 20, 30], &[11, 12, 21], &[111, 211, 212], &[1111]];
        hn.story(1, "story", levels[0]);
        let kids: [(u64, &[u64]); 10] = [
            (10, &[11, 12]),
            (20, &[21]),
            (30, &[]),
            (11, &[111]),
            (12, &[]),
            (21, &[211, 212]),
            (111, &[1111]),
            (211, &[]),
            (212, &[]),
            (1111, &[]),
        ];
        for (id, k) in kids {
            hn.comment(id, "octocat", "text", k);
        }
        // Held, so the second level waits on it even though its siblings,
        // and the parents of every second-level comment, are long back.
        hn.delay("/v0/item/30.json", Duration::from_millis(2500));
        let c = client(&hn);
        let fetch = std::thread::spawn(move || c.thread(1));

        let asked = |id: u64| hn.hits(&format!("/v0/item/{id}.json")) > 0;
        assert!(wait_for(Duration::from_secs(5), || asked(30)));
        std::thread::sleep(Duration::from_millis(700));
        let early: Vec<u64> = levels[1..]
            .iter()
            .flat_map(|l| l.iter().copied())
            .filter(|&id| asked(id))
            .collect();
        assert!(
            early.is_empty(),
            "asked for {early:?} before level 1 was in"
        );

        let (_, comments) = fetch.join().unwrap().unwrap();
        assert_eq!(
            outline(&comments),
            [
                (10, 0),
                (11, 1),
                (111, 2),
                (1111, 3),
                (12, 1),
                (20, 0),
                (21, 1),
                (211, 2),
                (212, 2),
                (30, 0)
            ]
        );
        // Each once, and every level asked for before any of the next.
        let log = hn.requests();
        let at = |id: u64| {
            let path = format!("/v0/item/{id}.json");
            let found: Vec<usize> = (0..log.len()).filter(|&i| log[i] == path).collect();
            assert_eq!(found.len(), 1, "item {id} asked for {} times", found.len());
            found[0]
        };
        for pair in levels.windows(2) {
            let last = pair[0].iter().map(|&id| at(id)).max().unwrap();
            let first = pair[1].iter().map(|&id| at(id)).min().unwrap();
            assert!(
                last < first,
                "{:?} overlaps {:?}: {log:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_firebase_comment_that_fails_to_load_loses_only_its_own_branch() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12, 13, 14]);
        hn.comment(11, "octocat", "a", &[21, 22, 23, 24]);
        hn.route("/v0/item/21.json", 500, "oops");
        // 22 is not routed: a 404.
        hn.route("/v0/item/23.json", 200, "null");
        hn.route("/v0/item/24.json", 200, "<html>");
        hn.comment(12, "octocat", "b", &[25]);
        hn.comment(25, "octocat", "c", &[31, 32]);
        hn.route("/v0/item/31.json", 500, "oops");
        hn.comment(32, "octocat", "d", &[]);
        hn.route("/v0/item/13.json", 500, "oops");
        hn.comment(14, "octocat", "e", &[]);
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(
            outline(&comments),
            [(11, 0), (12, 0), (25, 1), (32, 2), (14, 0)]
        );
    }

    #[test]
    fn a_firebase_level_that_fails_entirely_still_shows_the_levels_above() {
        let hn = FakeHn::start();
        hn.story(1, "story", &[11, 12]);
        hn.comment(11, "octocat", "a", &[21]);
        hn.comment(12, "octocat", "b", &[22]);
        hn.route("/v0/item/21.json", 500, "oops");
        hn.route("/v0/item/22.json", 500, "oops");
        let (_, comments) = client(&hn).thread(1).unwrap();
        assert_eq!(outline(&comments), [(11, 0), (12, 0)]);
    }

    // ------------------------------------------------------------- deep trees

    #[test]
    fn an_algolia_tree_thousands_deep_loads_on_a_worker_sized_stack() {
        // A debug build fits about 14,000 on 64 MiB (a release build 97,000,
        // and 3,000 on a default 2 MiB thread), so this leaves headroom.
        const DEPTH: usize = 5_000;
        let hn = FakeHn::start();
        hn.story(1, "deep", &[1_000_000]);
        hn.route("/algolia/v1/items/1", 200, algolia_chain(1, DEPTH));
        let c = client(&hn);
        // Parsed, turned into comments and dropped, all on the worker's stack.
        let depth = on_stack(WORKER_STACK, move || chain_len(&c.thread(1).unwrap().1));
        assert_eq!(hn.hits("/v0/item/1000000.json"), 0, "fell back to Firebase");
        assert_eq!(depth, DEPTH);
    }

    #[test]
    fn a_tree_nested_past_the_limit_is_an_error_not_a_crash() {
        // Deep enough to overflow even a worker's stack if it were parsed.
        let hn = FakeHn::start();
        hn.story(1, "deep", &[1_000_000]);
        hn.route("/algolia/v1/items/1", 200, algolia_chain(1, 300_000));
        let c = client(&hn);
        // On a default 2 MiB stack: the check comes before any recursion.
        let (_, comments) = c.thread(1).unwrap();
        assert!(comments.is_empty());
        assert_eq!(hn.hits("/v0/item/1000000.json"), 1, "fell back to Firebase");
        assert_eq!(nesting(&algolia_chain(1, 10)), 22);
    }

    #[test]
    fn nesting_skips_brackets_inside_strings() {
        assert_eq!(nesting(r#"{"a":"[[[{{\"]]"}"#), 1);
        assert_eq!(nesting(r#"[[1],[[2]]]"#), 3);
        assert_eq!(nesting(r#""\\" [ ]"#), 1);
        assert_eq!(nesting(""), 0);
    }

    #[test]
    fn one_malformed_search_hit_drops_out_and_the_rest_stay() {
        let hn = FakeHn::start();
        hn.search(json!([
            {"objectID": "1", "title": "kept"},
            {"title": "no id"},
            {"objectID": "2", "title": "wrong type", "points": "many"},
            {"objectID": "3", "title": "also kept"},
        ]));
        let titles: Vec<String> = client(&hn)
            .search("x")
            .unwrap()
            .into_iter()
            .map(|s| s.title)
            .collect();
        assert_eq!(titles, ["kept", "also kept"]);
    }

    #[test]
    fn a_body_that_is_not_json_names_the_url_it_came_from() {
        let hn = FakeHn::start();
        hn.route("/v0/topstories.json", 200, "<html>");
        let e = format!("{:#}", client(&hn).feed(Feed::Top).unwrap_err());
        assert!(e.contains("/v0/topstories.json"), "{e}");
    }

    #[test]
    fn a_firebase_chain_hundreds_deep_loads_on_a_worker_sized_stack() {
        const DEPTH: u64 = 300;
        let hn = FakeHn::start();
        hn.story(1, "deep", &[100]);
        for id in 100..100 + DEPTH {
            let kids: &[u64] = if id + 1 < 100 + DEPTH { &[id + 1] } else { &[] };
            hn.comment(id, "octocat", "reply", kids);
        }
        let c = client(&hn);
        let depth = on_stack(WORKER_STACK, move || chain_len(&c.thread(1).unwrap().1));
        assert_eq!(depth, DEPTH as usize);
        // The story, Algolia, and each comment once.
        assert_eq!(hn.requests().len(), DEPTH as usize + 2);
    }

    // ---------------------------------------------------------------- links

    #[test]
    fn domain_keeps_ports_and_ipv6_brackets_and_needs_a_scheme() {
        assert_eq!(
            at_url("https://example.com:8080/a").domain(),
            Some("example.com:8080")
        );
        assert_eq!(
            at_url("http://[2001:db8::1]:8080/x").domain(),
            Some("[2001:db8::1]:8080")
        );
        assert_eq!(at_url("http://[::1]/").domain(), Some("[::1]"));
        assert_eq!(
            at_url("https://example.com?q=1").domain(),
            Some("example.com")
        );
        assert_eq!(
            at_url("https://example.com#top").domain(),
            Some("example.com")
        );
        assert_eq!(at_url("ftp://example.com/f").domain(), Some("example.com"));
        assert_eq!(at_url("example.com/a").domain(), None);
        assert_eq!(at_url("//example.com/a").domain(), None);
        assert_eq!(at_url("mailto:octocat@example.com").domain(), None);
    }

    #[test]
    fn domain_strips_one_lowercase_www_and_nothing_else() {
        assert_eq!(
            at_url("https://www.www.example.com/").domain(),
            Some("www.example.com")
        );
        assert_eq!(
            at_url("https://wwwexample.com/").domain(),
            Some("wwwexample.com")
        );
        assert_eq!(
            at_url("https://www2.example.com/").domain(),
            Some("www2.example.com")
        );
        // Case-sensitive, though hosts are not.
        assert_eq!(
            at_url("https://WWW.Example.COM/").domain(),
            Some("WWW.Example.COM")
        );
    }

    #[test]
    fn domain_is_the_host_not_the_credentials_in_front_of_it() {
        assert_eq!(
            at_url("https://octocat:hunter2@example.com/a").domain(),
            Some("example.com")
        );
        // Shown as it is now, the name before the `@` passes for the site.
        assert_eq!(
            at_url("https://news.ycombinator.com@example.com/").domain(),
            Some("example.com")
        );
    }

    #[test]
    fn link_is_the_article_or_else_the_discussion() {
        let text_post = Story {
            id: 8863,
            ..Story::default()
        };
        assert_eq!(
            text_post.hn_url(),
            "https://news.ycombinator.com/item?id=8863"
        );
        assert_eq!(text_post.link(), text_post.hn_url());
        let article = Story {
            id: 8863,
            url: Some("https://example.com/a?b=c#d".into()),
            ..Story::default()
        };
        assert_eq!(article.link(), "https://example.com/a?b=c#d");
        assert_eq!(
            article.hn_url(),
            "https://news.ycombinator.com/item?id=8863"
        );
    }

    /// Against the real APIs: `cargo test -- --ignored`.
    #[test]
    #[ignore = "hits the network"]
    fn live_endpoints() {
        let c = Client::new();
        let top = c.feed(Feed::Top).unwrap();
        let page = c.stories(&top[..10]).unwrap();
        assert!(page.len() >= 8 && page.iter().all(|s| !s.title.is_empty()));

        let (story, comments) = c.thread(page[0].id).unwrap();
        assert_eq!(story.id, page[0].id);
        assert!(!comments.is_empty() || story.comments == 0);

        // The newest story with replies is usually not in Algolia yet, which
        // is the Firebase path.
        let new = c.feed(Feed::New).unwrap();
        let fresh = c
            .stories(&new[..30])
            .unwrap()
            .into_iter()
            .find(|s| s.comments > 0)
            .expect("a new story with a reply");
        let (_, comments) = c.thread(fresh.id).unwrap();
        assert!(!comments.is_empty(), "story {} lost its replies", fresh.id);

        assert!(!c.search("rust").unwrap().is_empty());
    }
}
