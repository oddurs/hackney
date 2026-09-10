//! A stand-in for Hacker News: Firebase and Algolia served from one local
//! port, answering from routes a test sets up, and logging every request.
//!
//! Shared by the unit tests (included from `src/main.rs`) and the end-to-end
//! tests (which point the real binary at it through `HACKNEY_FIREBASE_URL`
//! and `HACKNEY_ALGOLIA_URL`). Plain `std`, one thread per connection, and
//! every response closes its connection, so there is nothing to keep alive.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// Held this long before answering: a slow or hung server.
    pub delay: Duration,
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Reply>,
    /// Every request's path and query, in arrival order.
    log: Vec<String>,
}

pub struct FakeHn {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl FakeHn {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(conn, &state));
            }
        });
        FakeHn { addr, state }
    }

    /// Base URL to hand the client as Firebase's.
    pub fn firebase(&self) -> String {
        format!("http://{}/v0", self.addr)
    }

    /// Base URL to hand the client as Algolia's.
    pub fn algolia(&self) -> String {
        format!("http://{}/algolia/v1", self.addr)
    }

    /// Answer `path` (no query string) with `status` and `body`.
    pub fn route(&self, path: &str, status: u16, body: impl Into<String>) {
        self.state.lock().unwrap().routes.insert(
            path.into(),
            Reply {
                status,
                body: body.into(),
                delay: Duration::ZERO,
            },
        );
    }

    pub fn json(&self, path: &str, value: &Value) {
        self.route(path, 200, value.to_string());
    }

    /// Hold the answer to `path` for `delay`.
    pub fn delay(&self, path: &str, delay: Duration) {
        if let Some(r) = self.state.lock().unwrap().routes.get_mut(path) {
            r.delay = delay;
        }
    }

    pub fn remove(&self, path: &str) {
        self.state.lock().unwrap().routes.remove(path);
    }

    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// How many requests have been made for `path`, ignoring the query.
    pub fn hits(&self, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.split('?').next() == Some(path))
            .count()
    }

    // ------------------------------------------------------- HN-shaped data

    /// A feed endpoint, e.g. `feed("topstories", &[1, 2, 3])`.
    pub fn feed(&self, name: &str, ids: &[u64]) {
        self.json(&format!("/v0/{name}.json"), &json!(ids));
    }

    /// A Firebase item; its path comes from its `id`.
    pub fn item(&self, item: Value) {
        let id = item["id"].as_u64().expect("an item has an id");
        self.json(&format!("/v0/item/{id}.json"), &item);
    }

    /// A story with neutral data, `kids` its top-level comments.
    pub fn story(&self, id: u64, title: &str, kids: &[u64]) {
        self.item(story_json(id, title, kids));
    }

    /// A Firebase comment.
    pub fn comment(&self, id: u64, by: &str, text: &str, kids: &[u64]) {
        self.item(json!({
            "id": id, "type": "comment", "by": by, "text": text,
            "time": now() - 600, "kids": kids,
        }));
    }

    /// Algolia's whole tree for a story.
    pub fn tree(&self, id: u64, children: Value) {
        self.json(
            &format!("/algolia/v1/items/{id}"),
            &json!({"id": id, "author": "octocat", "children": children}),
        );
    }

    /// Algolia search results for any query.
    pub fn search(&self, hits: Value) {
        self.json("/algolia/v1/search", &json!({ "hits": hits }));
    }
}

/// An Algolia tree node.
pub fn node(id: u64, author: &str, text: &str, children: Value) -> Value {
    json!({
        "id": id, "author": author, "text": text,
        "created_at_i": now() - 600, "children": children,
    })
}

pub fn story_json(id: u64, title: &str, kids: &[u64]) -> Value {
    json!({
        "id": id, "type": "story", "by": "octocat", "title": title,
        "url": format!("https://example.com/{id}"), "score": 100 + id,
        "time": now() - 3600, "descendants": kids.len(), "kids": kids,
    })
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn serve(conn: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Headers, up to the blank line. GET requests have no body.
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        line.clear();
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let reply = {
        let mut s = state.lock().unwrap();
        s.log.push(target);
        s.routes.get(&path).cloned()
    };
    let reply = reply.unwrap_or(Reply {
        status: 404,
        body: r#"{"error":"not found"}"#.into(),
        delay: Duration::ZERO,
    });
    std::thread::sleep(reply.delay);
    let mut conn = conn;
    let _ = write!(
        conn,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = conn.flush();
}
