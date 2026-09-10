# Contributing

Thanks for helping. Bug reports and small, focused pull requests are the most
useful things you can send.

## Once, after cloning

```sh
scripts/setup
```

This points `core.hooksPath` at `.githooks/` and checks for the tools the suite
needs: the Rust toolchain pinned in `rust-toolchain.toml`, `cargo-nextest` and
`shellcheck`.

## The loop

```sh
scripts/task check    # fmt:check, lint, test, build: exactly what CI runs
cargo nextest run <filter>    # iterate on a few tests
```

The hooks run formatting and lint on commit and the whole check on push.
Commit messages are [Conventional Commits](https://www.conventionalcommits.org)
(`feat:`, `fix:`, `docs:` …), 72 characters at most, with no trailing period.

## Tests

- Every behaviour change comes with a test that fails without it. Run the test
  against the old code, or break the new code on purpose, and watch it fail.
- Tests live beside the code they cover, except `tests/e2e.rs`, which drives
  the real binary in a pseudo-terminal. Both use the fake Hacker News in
  `tests/support/fake_hn.rs`; the binary is pointed at it with
  `HACKNEY_FIREBASE_URL` and `HACKNEY_ALGOLIA_URL`.
- A failing snapshot means the screen changed. Read each diff in
  `cargo insta review` and accept only what you meant. Never accept in bulk,
  and never commit a `.snap.new`.
- Never add `#[ignore]`, retries or longer timeouts to get past a failing or
  flaky test; find out why it fails. The `live_` test is the only ignored one,
  because it needs the network (`scripts/task test:live`).
- End-to-end tests wait on something only the target state draws.

## Screenshots

`scripts/screenshots` rebuilds `docs/screenshots/` with
[vhs](https://github.com/charmbracelet/vhs), against the demo server in
`examples/demo_server.rs`. Re-run it when the screen changes in a way the
README shows.

## Releasing

1. Move the `[Unreleased]` notes in `CHANGELOG.md` under the new version, and
   bump `version` in `Cargo.toml`.
2. Merge that, then tag it: `git tag v0.2.0 && git push origin v0.2.0`.

The tag runs the whole check, builds the binaries, publishes the GitHub
release with that version's changelog section, and publishes to crates.io. The
Homebrew tap picks the release up on its own.
