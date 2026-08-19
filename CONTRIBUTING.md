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

## Release checklist

1. Update `version` in `Cargo.toml` and `herdr-plugin.toml`.
2. Update `Cargo.lock` with `cargo check`.
3. Move user-visible changes into a dated section in `CHANGELOG.md`.
4. Run the local checks above on a clean checkout.
5. Test installation or linking with the oldest supported Herdr version when
   compatibility-sensitive manifest or API behavior changed.
6. Commit the release and create an annotated `vX.Y.Z` tag whose version matches
   both manifests.
7. Push the commit and tag. The release workflow validates the tag and creates
   the GitHub release.
8. Reinstall from GitHub and invoke each action:

   ```bash
   herdr plugin install SpaceK33z/herdr-worktrees --yes
   herdr plugin action invoke open --plugin worktrees
   herdr plugin action invoke open-base --plugin worktrees
   herdr plugin action invoke remove --plugin worktrees
   ```

9. Confirm the repository has the `herdr-plugin` GitHub topic so it appears in
   the Herdr marketplace.

The plugin is distributed as source. Herdr clones the tagged repository and
runs `cargo build --release`; GitHub releases do not contain prebuilt binaries.
