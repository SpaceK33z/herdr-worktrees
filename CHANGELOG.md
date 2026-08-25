# Changelog

Notable user-visible changes are recorded here. This project follows
[Semantic Versioning](https://semver.org/).

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
