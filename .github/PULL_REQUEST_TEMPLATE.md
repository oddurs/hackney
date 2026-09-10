## Problem

<!-- What was wrong or missing. Link the issue if there is one. -->

## Approach

<!-- What you changed and why this way. Note anything you rejected. -->

## Look at this skeptically

<!-- Where a reviewer should push hardest: the assumption you are least sure
     of, the edge case you could not test, the thing that will break first. -->

## Checklist

- [ ] `scripts/task check` passes locally
- [ ] Behaviour change is covered by a test that would have failed before
- [ ] Screen changes: snapshots reviewed one by one in `cargo insta review`
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` if user-visible
- [ ] No assistant attribution anywhere in the commits or this description
