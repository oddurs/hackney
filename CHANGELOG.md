# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-10

The first release.

### Added

- A two-pane reader: the Top, New, Best, Ask, Show and Jobs feeds beside the
  selected story's comment thread, stacked below 120 columns, and zoomed to
  full width with `z`.
- Comments load when the cursor settles on a story, and the next few are
  fetched ahead of it. Visited tabs open at once, and refreshing keeps the list
  on screen and the cursor on the same story.
- Threads with nesting rails, folding (`x`, or a click on a comment's name),
  jumping between top-level threads (`n`/`p`), the poster's replies
  highlighted, and quotes, code blocks and clickable links.
- Search across Hacker News (`/`), via Algolia.
- Mouse support: tabs, stories, links and comment headers are clickable; the
  wheel scrolls the list without moving the selection.
- Colours come from the terminal's own palette, with the selection band mixed
  from its real foreground and background. `NO_COLOR` is honoured.
- `HACKNEY_OPENER` picks the program links open with.
- `--help` and `--version`.

[Unreleased]: https://github.com/oddurs/hackney/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/oddurs/hackney/releases/tag/v0.1.0
