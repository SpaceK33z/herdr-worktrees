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

Fully automated with
[release-please](https://github.com/googleapis/release-please). Commit with
[conventional commit](https://www.conventionalcommits.org/) messages — `fix:`
bumps the patch version, `feat:` the minor, and `feat!:`/`BREAKING CHANGE:`
the major. On every push to main, release-please opens or updates a release PR
that bumps `Cargo.toml` and `herdr-plugin.toml`, dates a new `CHANGELOG.md`
section generated from the commit subjects, and records the released version.
Merging that PR cuts the release: it tags `vX.Y.Z`, publishes the GitHub
release, and re-runs the full check suite (fmt, clippy, tests, build, package)
against the released commit on Linux and macOS.

After a release, verify the install path:

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
