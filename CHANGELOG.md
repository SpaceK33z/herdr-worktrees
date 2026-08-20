# Changelog

Notable user-visible changes are recorded here. This project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `.worktreeinclude` support: gitignored files named by that file — `.env`, a
  local secrets file, a dependency directory — are copied into a new worktree
  before the setup script runs, reflinked where the filesystem allows it.
  `herdr-worktrees include` shows what a new worktree would receive, and
  `worktree-include = false` turns it off.
- `fetch-before-create` (default `true`): refresh the base's remote-tracking ref
  before creating a branch from it, so new work starts from the upstream tip. A
  failed or slow fetch falls back to the local copy instead of blocking.

### Fixed

- Pull request columns and the `merged` state stayed empty in repositories with
  a long pull request history. The background refresh paged through every pull
  request the repository ever had, which no timeout can wait out; it now lists
  the open and most recently merged ones and asks about the branches those
  listings miss one at a time, checked-out branches first.
- The remove picker draws its pull request columns from the cache on the first
  frame and refreshes them with the reload behind it, instead of leaving them
  empty and never showing that a worktree's branch was merged.

## [0.2.0] - 2026-08-19

### Added

- Push, pull, diverged, local, remote, gone, and merged branch states.
- Checkout support for `origin` branches that do not exist locally yet.
- Batched removal with multi-select, safety labels, progress, and estimated
  reclaimed disk space.
- Pull request lookup for all visible branches in one background request.
- Herdr theme colors for worktree and branch sections.
- `ctrl-n` to create the typed branch when a fuzzy match is highlighted.

### Changed

- The picker renders local metadata first and fills in slower checks in the
  background.
- Removal guards now distinguish dirty, unpublished, and detached worktrees.
- Merged pull request state is shown only when it matches the branch's current
  commit.
- The minimum supported versions are now Rust 1.87 and fzf 0.71.

[0.2.0]: https://github.com/SpaceK33z/herdr-worktrees/releases/tag/v0.2.0
