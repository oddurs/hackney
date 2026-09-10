<div align="center">

# hackney

**Hacker News in the terminal.**

A fast two-pane reader for stories and comment threads — driven by the keyboard,
happy with the mouse, and dressed in your terminal's own colours.

[![CI](https://github.com/oddurs/hackney/actions/workflows/ci.yml/badge.svg)](https://github.com/oddurs/hackney/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/oddurs/hackney?color=ff6600)](https://github.com/oddurs/hackney/releases/latest)
[![crates.io](https://img.shields.io/crates/v/hackney?color=ff6600)](https://crates.io/crates/hackney)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/demo.gif" alt="hackney: moving through the front page, folding a thread, switching feeds and searching" width="100%">

</div>

## Why

Reading Hacker News is mostly two things: skimming a list, and reading the one
thread worth reading. `hackney` puts both on one screen. The list stays on the
left, the thread you're on fills the right, and moving through the list swaps
the comments in as fast as you can press `j`.

## Features

- **Two panes, one glance.** The front page beside the thread you're reading.
  Below 120 columns the two stack; `z` gives the comments the whole screen.
- **Feels instant.** Comments load when the cursor settles on a story, and the
  next few are fetched ahead of you. Tabs you've visited open at once, and
  refreshing keeps the list on screen and your cursor on the same story.
- **Threads that read well.** Nested replies hang from guide rails. `x` folds
  the thread you're in and `n`/`p` jump between threads. The poster's replies
  are highlighted, and quotes, code blocks and links are all drawn properly.
- **Your colours, not ours.** There's no bundled theme. Everything is drawn from
  your terminal's palette, and the selection band is mixed from its real
  foreground and background, so it looks at home in any scheme.
  `NO_COLOR` is honoured.
- **Mouse too.** Click tabs, stories, links and comment headers. The wheel
  scrolls the list without moving your place.
- **Search** all of Hacker News from `/`.
- **Small and read-only.** One binary, no config file, no account. It only
  reads the public APIs, and only ever hands `http(s)` links to your browser.

## Install

**Homebrew** (macOS and Linux)

```sh
brew install oddurs/tap/hackney
```

**Cargo**

```sh
cargo install hackney
```

**Prebuilt binaries** for macOS (Apple Silicon and Intel) and Linux (x86_64 and
arm64) are attached to every [release](https://github.com/oddurs/hackney/releases/latest),
each with a SHA-256 checksum:

```sh
# e.g. Apple Silicon; see the release for the other targets
curl -L https://github.com/oddurs/hackney/releases/latest/download/hackney-aarch64-apple-darwin.tar.gz | tar xz
./hackney
```

## Usage

```sh
hackney
```

| Keys | |
|---|---|
| `j` `k` / `↓` `↑` | move through stories |
| `g` `G` | first / last story |
| `1`–`7`, `Tab` | switch feed: Top, New, Best, Ask, Show, Jobs, Search |
| `Enter` / `o` | open the article in your browser |
| `c` | open the discussion on news.ycombinator.com |
| `J` `K`, `Space` `b` | scroll the comments by line / page |
| `n` `p` | next / previous top-level thread |
| `x` | fold the thread you're reading |
| `z` | comments full width |
| `/` | search Hacker News |
| `r` | refresh |
| `?` | every key, in the app |
| `q` | quit |

| Mouse | |
|---|---|
| click a tab | switch feed |
| click a story | read its comments |
| double-click a story | open the article |
| click a comment's name | fold its replies |
| click a link | open it in the browser |
| scroll wheel | scroll the list, or the comments |

## Screenshots

<table>
  <tr>
    <td width="50%"><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/front.png" alt="The front page beside the first story's comments"></td>
    <td width="50%"><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/light.png" alt="The same screen in a light terminal theme"></td>
  </tr>
  <tr>
    <td align="center">The front page and a thread</td>
    <td align="center">…in a light terminal: the colours are yours</td>
  </tr>
  <tr>
    <td><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/zoom.png" alt="A thread zoomed to full width, centred"></td>
    <td><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/stacked.png" alt="List above comments in a narrower terminal"></td>
  </tr>
  <tr>
    <td align="center"><code>z</code>: a thread at full width</td>
    <td align="center">Under 120 columns, the panes stack</td>
  </tr>
  <tr>
    <td><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/ask.png" alt="An Ask HN post with its text above the comments"></td>
    <td><img src="https://raw.githubusercontent.com/oddurs/hackney/main/docs/screenshots/help.png" alt="The key help over the reader"></td>
  </tr>
  <tr>
    <td align="center">Ask HN, text first</td>
    <td align="center"><code>?</code>: every key</td>
  </tr>
</table>

<sub>Screenshots use made-up stories from the demo server (`cargo run --example demo_server`), so no one's real posts are reproduced here.</sub>

## How it works

Feeds and stories come from the official [Firebase API](https://github.com/HackerNews/API).
A story's whole comment tree comes from [Algolia](https://hn.algolia.com/api)
in a single request and is put back into Hacker News' own ranking. For stories
too new for Algolia's index, it falls back to walking the tree in Firebase,
one round of parallel requests per level of nesting. Search is Algolia too.

Rendering is done with [ratatui](https://ratatui.rs). The comments pane is
wrapped once and cached, so scrolling a thread of thousands of comments costs
only the lines on screen.

## Configuration

There's no config file. Two environment variables:

| Variable | |
|---|---|
| `HACKNEY_OPENER` | the program links are handed to (default: `open` on macOS, `xdg-open` elsewhere) |
| `NO_COLOR` | draw without colour ([no-color.org](https://no-color.org)) |

## Development

```sh
scripts/setup         # once: git hooks and tool check
scripts/task check    # everything CI runs: fmt, clippy, shellcheck, tests, build
```

The suite has unit, property, snapshot and end-to-end tests. The end-to-end
tests drive the real binary in a pseudo-terminal against a local fake of both
APIs. Everything runs offline except `scripts/task test:live`, which checks the
real APIs. `scripts/screenshots` regenerates the images above with
[vhs](https://github.com/charmbracelet/vhs). See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE). Not affiliated with Y Combinator or Hacker News.
