# Changelog

Notable user-visible changes are recorded here. This project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The removal picker now explains its safety verdict. A dimmed tag after the
  safety column says why a branch is deletable: `·pushed` (every commit is on
  the upstream) or `·merged` (the content already landed on the base branch —
  squash merge or rebase — even though git ancestry cannot show it). The tag
  also refines the verdict itself: a branch the sync column calls unpublished
  but whose content is proven merged shows `✓ safe ·merged` instead of a
  warning.

- Safer automatic branch deletion. When `remove.delete-branch` is on, a branch
  whose upstream is gone or stale — the normal state after a GitHub squash
  merge — is now still deleted without a confirmation prompt when its content
  has already landed on the base branch. Content integration is detected by
  cheap plumbing probes, in order of cost: same commit as the base, contained
  in the base's history, no file changes vs. the base, identical trees, or a
  simulated merge (`git merge-tree`) that would add nothing.
- A branch still checked out in another worktree is never deleted, regardless
  of merge status or confirmation: deleting the ref would leave that worktree
  unable to resolve `HEAD`. The removal summary names the surviving checkout.
- Squash merges that later conflict are still recognized. When a branch was
  squash-merged and the base branch afterwards changed the same files, the
  simulated merge conflicts — previously that read as unmerged work. The
  branch's combined diff is now hashed (`git patch-id`) against every commit
  on the base since the merge point; an exact match proves the squash landed,
  so the branch is deleted without a confirmation prompt. The walk is capped
  at 500 base commits; beyond that the conservative "unmerged" answer stands.

- `herdr-worktrees create <branch>`: the popup's creation pipeline as a plain
  CLI command, for coding agents and scripts. Applies `branch-prefix`, the
  `worktree-path` template, base resolution, and fetch-before-create, then
  copies `.worktreeinclude` entries and runs the setup script synchronously.
  Supports `--base`, `--exact`, `--json`, and `--no-setup`.
- `.worktreeinclude` support: gitignored files named by that file — `.env`, a
  local secrets file, a dependency directory — are copied into a new worktree
  before the setup script runs, reflinked where the filesystem allows it.
  `herdr-worktrees include` shows what a new worktree would receive, and
  `worktree-include = false` turns it off.
- `fetch-before-create` (default `true`): refresh the base's remote-tracking ref
  before creating a branch from it, so new work starts from the upstream tip. A
  failed or slow fetch falls back to the local copy instead of blocking.

### Changed

- GitHub pull request columns and merged state are now enabled by default. Set
  `github-prs = false` to opt out.

### Fixed

- `{{ user }}` expanded to the raw git `user.name`, so a full name like
  "Kees Kluskens" produced an invalid branch prefix and creation failed. The
  name is now sanitized to one safe token (`Kees-Kluskens`).
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
