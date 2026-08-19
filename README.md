# herdr-worktrees

A [Herdr](https://herdr.dev) plugin for switching, creating, and removing Git
worktrees from a fuzzy popup. It lists checked-out worktrees, local branches,
and remote-only `origin` branches with worktree name, pull request, review,
unresolved review threads, conflict, age, working tree, and push/pull state.

```text
┌ worktrees ──────────────────────────────────────────────────────────────────────────────────────────┐
│ > kees/                                                         3/7                                 │
│   WORKTREES    BRANCHES                                                                             │
│   branch            worktree        pr        review    threads  conflict  when   changes  sync     │
│ ▸ kees/parser-fix   parser-fix      #1234     approved  2                  2h     +3 ~1     ↑2      │
│   main              herdr-worktrees —         —         —                  4h     clean     ↓1      │
│   kees/queue-retry  —               #1235     —         —                  1d     —         merged  │
│   kees/old-spike    —               —         —         —                  3w     —         local   │
│ enter switch/create · ctrl-n new · alt+enter base… · ctrl-p open PR · GitHub: now                   │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## Requirements

- Herdr 0.7.4 or newer
- macOS or Linux
- `git`, `fzf` 0.71 or newer, and Rust 1.87 or newer with `cargo`
- Optional: [GitHub CLI](https://cli.github.com/) (`gh`) for pull request data

Herdr installs this plugin from source and runs `cargo build --release`.

## Install

```bash
herdr plugin install SpaceK33z/herdr-worktrees
```

Add the actions to `~/.config/herdr/config.toml`:

```toml
[keys]
new_worktree = ""             # remove Herdr's built-in prefix+shift+g binding

[[keys.command]]
key = "prefix+w"
type = "plugin_action"
command = "worktrees.open"
description = "worktree: switch or create"

[[keys.command]]
key = "prefix+shift+w"
type = "plugin_action"
command = "worktrees.open-base"
description = "worktree: create from a chosen base"

[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "worktrees.remove"
description = "worktree: delete"
```

Validate and reload the config:

```bash
herdr config check
herdr server reload-config
```

Press `prefix+w` in a Git workspace to open the picker.

## Key bindings

| Key | Action |
| --- | --- |
| `prefix+w` | Open the worktree picker. |
| `enter` | Run the highlighted action: switch worktrees, check out a local or remote-only branch, check out a pull request, or create the exact typed query from its create row. |
| `ctrl-n` | Create the exact typed query directly (shortcut for the create row). |
| `alt+enter` | Choose a custom base branch, then create. |
| `prefix+shift+w` | Open the picker in custom-base mode. |
| `prefix+d` | Open the removal picker. |
| `tab` / `shift-tab` | Select or deselect worktrees in the removal picker. |
| `ctrl-p` | Open the highlighted branch's pull request. |
| `ctrl-d` | Remove the highlighted worktree from the main picker. |
| `ctrl-r` | Recompute local and remote-tracking metadata. |
| `esc` | Close the popup without changing the layout. |

## What the picker shows

Checked-out **worktrees** appear first, followed by local **branches** without a
worktree, then remote-only branches from `origin`. Remote rows are displayed as
`origin/<branch>` and are omitted when a matching local branch exists; symbolic
remote entries such as `origin/HEAD` are not shown. Selecting a worktree switches
to it. Selecting a local branch creates a worktree for it. Selecting a remote row
creates local `<branch>` with `origin/<branch>` as its explicit upstream, uses the
normal local-branch worktree path, opens it in Herdr, and runs the configured
setup flow. Remote rows are checkout candidates only and are never added to the
removal picker. As soon as the query is nonempty, a bold, theme-colored
**create worktree** row appears last, below the fuzzy-ranked real matches, for
that exact text. Typing a pull-request reference (`123`, `#123`, or `pr:123`)
instead shows a **checkout pull request** row first, above the matches, so
pressing `enter` on a bare number is deterministic. Branch names stay neutral
so the active Herdr palette does not assign arbitrary semantic colors to
worktrees and branches.

The picker draws batched local and `origin` ref metadata first and updates the
list after working tree, sync, and GitHub checks finish. Local rows remain first;
remote discovery does not start one Git process per row. A `…` in **changes** or
**sync** means the background scan is still running.

- **worktree**: Final directory name for a checked-out worktree. Branch rows show `—`.
  Set `show-worktree-name = false` to hide this column.
- **pr**: Pull request number. Ctrl-click the link or press `ctrl-p` to open it.
- **review**: `approved`, `changes`, `review`, or `draft`.
- **threads**: Number of unresolved GitHub review threads. `0` means all threads
  are resolved, `?` means the thread lookup failed, and `N+` means the pull
  request has more than 100 review threads.
- **conflict**: `conflict` when an open pull request has merge conflicts.
- **when**: Age of the branch's latest commit.
- **changes**: `+staged ~unstaged` or `clean`. The list skips untracked files for
  speed, but removal performs a full status check before deleting anything.
- **sync**: State relative to the configured upstream, or `origin/<branch>` when
  that same-named remote-tracking branch exists:
  - `↑N`: commits to push
  - `↓N`: commits to pull
  - `↑N ↓M`: local and remote have diverged
  - `—`: synchronized
  - `local`: no upstream or same-named remote branch
  - `remote`: the branch exists on `origin` but not locally
  - `gone`: the configured upstream no longer exists
  - `merged`: GitHub reports a merged pull request at the branch's current commit

Pull request columns and the `merged` state require `github-prs = true`, a
GitHub remote, and an authenticated `gh` CLI. GitHub results are cached for 60
seconds. The dim footer timestamp shows when GitHub data was last fetched;
`ctrl-r` updates it after a successful refresh. Pull counts use local
remote-tracking refs; run `git fetch` when you need current remote state.

## Creating a worktree

Type a branch name and select the **create worktree** row to create that exact
query even when existing entries fuzzy-match it. `ctrl-n` remains a direct
shortcut for the same operation. The plugin then:

1. Applies `branch-prefix` and resolves `worktree-path`.
2. Runs `git worktree add` from the configured base, the remote HEAD, `main` or
   `master`, or the current branch (in that order).
3. Opens the checkout in Herdr as a nested workspace or tab, depending on
   `open-mode`.
4. Runs `[pre-start].setup-worktree` asynchronously in the new checkout.

For an existing remote-only row, the remote branch name is used as-is: the
plugin does not apply `branch-prefix`. For example, selecting `origin/topic`
creates local `topic`, while `origin/kees/topic` creates local `kees/topic` and
keeps the usual prefix-aware `branch_short` path behavior.

The setup script receives `WORKTREE_PATH`, `WORKTREE_BRANCH`, `REPO_PATH`, and
`BASE_BRANCH`. A non-zero exit leaves the worktree in place and reports the
script output instead of silently continuing.

### Branch prefixes

Set `branch-prefix` to avoid typing the same namespace for every branch:

```toml
branch-prefix = "kees/"
branch-prefix = "{{ user }}/"     # git user.name, then $USER
branch-prefix = "u/{{ user }}/"
```

- An existing prefix is not duplicated.
- A leading `/` opts out once: `/hotfix-ci` creates `hotfix-ci`.
- Filtering ignores the prefix, so `parser` still matches `kees/parser-fix`.
- `worktree-path` can use `{{ branch }}` or the unprefixed
  `{{ branch_short }}`.

## Checking out a pull request

Type a pull-request reference — `123`, `#123`, or `pr:123` — and press `enter`
to check out that PR into a worktree. The plugin resolves the PR's real head
branch with `gh pr view` and never applies `branch-prefix` to it. If the branch
already has a worktree, it switches to it; otherwise it fetches the branch and
creates a worktree at the normal configured path, then opens it in Herdr and
runs the setup flow.

- A same-repository PR fetches `origin/<branch>` and creates a tracking worktree
  from it, so `git push` and the sync column work normally. An existing local
  branch is reused as-is.
- A fork PR fetches `refs/pull/N/head` into a temporary ref and creates a local
  branch from it. You are asked to confirm first, because the checkout runs the
  setup script against code you may not have reviewed. The branch is left
  without an upstream; pushing to a fork is not configured automatically.
- If a fork's branch name collides with a divergent local branch, the plugin
  stops with an error rather than overwriting it.

This works independently of `github-prs` (which only controls the background PR
columns). Set `pr-checkout = false` to disable the feature entirely, so a bare
number is treated as a literal branch name again.

## Removing worktrees

Press `prefix+d` to list every worktree except the main checkout. Rows appear
from Git metadata first with `… checking`, then update in place after the
untracked-file safety scan finishes. Entering before that scan completes runs
the same check on the selected worktrees before confirmation. The safety column
explains what removal would affect:

- Green `✓ safe`: no tracked or untracked changes. With `delete-branch = true`,
  the branch also has no unpublished commits.
- Red `⚠ dirty`: the worktree has uncommitted changes.
- Yellow `⚠ unpublished`: deleting the configured branch would discard commits
  that are not on its remote-tracking branch.
- Yellow `⚠ detached`: the worktree has a detached HEAD.

Use `tab` and `shift-tab` to select several worktrees, then press `enter`.
Removal runs in the background and reports progress in a temporary Herdr pane.
The estimated freed space is based on filesystem block counts and can be
affected by unrelated disk activity.

Warned rows require `ctrl-x` unless `[remove].force = true`. Local branches are
kept unless `delete-branch = true`.

## Configuration

Create the plugin config at:

```bash
$EDITOR "$(herdr plugin config-dir worktrees)/config.toml"
```

```toml
worktree-path = "{{ repo_path }}/.worktrees/{{ branch | sanitize }}"
base-branch = "main"          # fallback: remote HEAD, main/master, current branch
branch-prefix = ""            # for example, "kees/"
open-mode = "workspace"       # "workspace" or "tab"
github-prs = false            # PR, review threads, conflict, and merged state
pr-checkout = true            # checkout a PR by typing its number
show-worktree-name = true     # show the worktree directory name column

[popup]
width = "90%"
height = "70%"

[remove]
delete-branch = false
force = false                 # bypass dirty and unpublished guards

[pre-start]
setup-worktree = '''
primary="$(git worktree list --porcelain | awk '/^worktree / { sub(/^worktree /, ""); print; exit }')"
setup="$primary/scripts/setup-worktree.sh"

if [ -f "$setup" ]; then
  bash "$setup" "$PWD"
fi
'''
```

`worktree-path` supports `{{ repo_path }}`, `{{ repo_name }}`, `{{ branch }}`,
`{{ branch_short }}`, `{{ base }}`, and `{{ user }}`. The `sanitize` filter
replaces `/` and other unsafe characters with `-`. Configuration is read on
every invocation; no reload is needed.

The shape intentionally follows `~/.config/worktrunk/config.toml`, so existing
Worktrunk path and setup settings can be copied with little adjustment.

## Update or uninstall

Herdr v1 updates GitHub plugins by reinstalling them:

```bash
herdr plugin install SpaceK33z/herdr-worktrees --yes
```

To remove the plugin:

```bash
herdr plugin uninstall worktrees
```

## Troubleshooting

Confirm that Herdr loaded the manifest and actions:

```bash
herdr plugin list --plugin worktrees --json
herdr plugin action list --plugin worktrees
```

Inspect recent plugin errors:

```bash
herdr plugin log list --plugin worktrees --limit 20
```

If pull request columns stay empty, run `gh auth status`, confirm the repository
has a GitHub remote, and set `github-prs = true`. If push/pull counts look stale,
run `git fetch` in the repository.

## Development

Build and link a checkout instead of installing it:

```bash
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke open --plugin worktrees
```

`plugin link` does not run the manifest's `[[build]]` command. Rebuild after
source changes and re-link after manifest changes:

```bash
./scripts/dev-relink.sh
```

Run the local checks before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
cargo package --locked
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the release checklist.

## Security

Herdr plugins are not sandboxed. This plugin runs `git`, `fzf`, `gh`, `herdr`,
and your configured setup script with your user permissions. Review the source
and your `[pre-start].setup-worktree` command before installing. The dirty and
unpublished checks reduce accidental deletion; `force = true` bypasses them.

Report security issues privately through the repository's GitHub security
advisory page rather than a public issue.

## License

[MIT](LICENSE) © Kees Kluskens
