# Changelog

Notable user-visible changes are recorded here. This project follows
[Semantic Versioning](https://semver.org/).

## [0.3.0](https://github.com/SpaceK33z/herdr-worktrees/compare/v0.2.0...v0.3.0) (2026-09-17)


### Features

* add --yes/--force to remove for non-interactive use ([e61262b](https://github.com/SpaceK33z/herdr-worktrees/commit/e61262be8075b4974ac514f64a9e128110d923b6))
* add Herdr worktree manager ([4bb88b2](https://github.com/SpaceK33z/herdr-worktrees/commit/4bb88b2e41738322372efe1b4a82517d0c157517))
* attach CLI-created worktrees to the repo's Herdr workspace ([0cbe62f](https://github.com/SpaceK33z/herdr-worktrees/commit/0cbe62f6c2bbda77ee056dd104d22e21ceaa18d1))
* checkout pull requests by number in the worktree picker ([49da7d0](https://github.com/SpaceK33z/herdr-worktrees/commit/49da7d065579ffbce69650c9aa484684d4adb232))
* copy .worktreeinclude entries into new worktrees ([f0d2013](https://github.com/SpaceK33z/herdr-worktrees/commit/f0d2013ffc927a6423c18633c784fe9f972e4166))
* default remove.delete-branch to true ([d0bcd91](https://github.com/SpaceK33z/herdr-worktrees/commit/d0bcd9188d2013c566707d8d840e3083916180af))
* detect squash/rebase integration before deleting a branch ([6a0be1d](https://github.com/SpaceK33z/herdr-worktrees/commit/6a0be1df2f8b8f87bd74ed2f8fb4054402f325f7))
* enable GitHub PR integration by default ([459189c](https://github.com/SpaceK33z/herdr-worktrees/commit/459189c08f5d9917a9bf26895a7777f7889913d6))
* explain the removal picker's safety verdict ([104d789](https://github.com/SpaceK33z/herdr-worktrees/commit/104d78962483ec7024fe957e46bb89b752b3d2e1))
* herdr-worktrees create for agents and scripts ([a22fed7](https://github.com/SpaceK33z/herdr-worktrees/commit/a22fed7f74cc2abadafd0a5c77e66ee862d898d4))
* prepare herdr-worktrees v0.2.0 ([57c1e3a](https://github.com/SpaceK33z/herdr-worktrees/commit/57c1e3a386d797d80b43806993ccde9bdfc92623))
* recognize squash merges that later conflict via patch-id match ([32e743f](https://github.com/SpaceK33z/herdr-worktrees/commit/32e743f51bc124253c66d88ccd032b7225217f27))
* skip the checkout-PR action row when a row already shows the PR ([5c70a92](https://github.com/SpaceK33z/herdr-worktrees/commit/5c70a92c728335d5d429542ef34f70065086fcc9))
* update a worktree from the picker with ctrl-u ([4b9ec68](https://github.com/SpaceK33z/herdr-worktrees/commit/4b9ec68e6326f5969c1260bc40777363fd2d459f))


### Bug Fixes

* accept any spelling of a worktree path in remove ([ed81539](https://github.com/SpaceK33z/herdr-worktrees/commit/ed8153928694e3f80fd2221fcfb608b4ede20b11))
* expose worktree attachment IDs for agent pane reuse ([c8206c9](https://github.com/SpaceK33z/herdr-worktrees/commit/c8206c937529a6f5d890c31f1d2bd00f11673b08))
* fill pull request columns in long-lived repositories ([544dcf2](https://github.com/SpaceK33z/herdr-worktrees/commit/544dcf245a9b4f3a50ba75c4ba77ed04292ad5b1))
* harden worktree lifecycle and release validation ([1ede6bf](https://github.com/SpaceK33z/herdr-worktrees/commit/1ede6bfda2df930e9717fe941dd448f567f501a9))
* match the main checkout through symlinks and make git tests hermetic ([766d980](https://github.com/SpaceK33z/herdr-worktrees/commit/766d9808fa2ae9c291d5f792f321a8364087a296))
* sanitize {{ user }} so a full git user.name works as a prefix ([13612ab](https://github.com/SpaceK33z/herdr-worktrees/commit/13612abef598d0ba56449a0795b95961f8c08b30))
* satisfy latest stable Clippy ([bb751de](https://github.com/SpaceK33z/herdr-worktrees/commit/bb751debb0cf18d1471ab0912de8e4a13c486ec1))

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
