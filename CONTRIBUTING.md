# Contributing

## Local checks

Install Rust 1.87 or newer, `git`, and fzf 0.71 or newer. Herdr 0.7.4 or newer
is required for manual plugin checks.

Run these commands before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
cargo package --locked
```

To test the plugin in Herdr:

```bash
cargo build --release
herdr plugin link "$PWD"
herdr plugin action list --plugin worktrees
herdr plugin action invoke open --plugin worktrees
```

`herdr plugin link` does not run the manifest build step. Rebuild after source
changes. Re-link after changing `herdr-plugin.toml` because Herdr caches linked
manifests.

## Releasing

One command does everything — version bumps, lockfile, changelog dating,
tag, and push:

```bash
scripts/release.sh <major|minor|patch|x.y.z>
```

It refuses to run on a dirty tree, off main, out of sync with origin, or with
an empty Unreleased changelog section, so record user-visible changes there as
you go.

After pushing, the release workflow validates the tag against both manifests,
runs the local checks, and creates the GitHub release. Then verify the install
path:

```bash
herdr plugin install SpaceK33z/herdr-worktrees --yes
herdr plugin action invoke open --plugin worktrees
herdr plugin action invoke open-base --plugin worktrees
herdr plugin action invoke remove --plugin worktrees
```

The repository keeps the `herdr-plugin` GitHub topic so it appears in the
Herdr marketplace.

The plugin is distributed as source. Herdr clones the tagged repository and
runs `cargo build --release`; GitHub releases do not contain prebuilt binaries.
