# herdr-worktrees

A [Herdr](https://herdr.dev) plugin for switching, creating, and removing Git
worktrees from a fuzzy popup. It lists checked-out worktrees, local branches,
and remote-only `origin` branches with pull request, review, conflict, age,
working tree, and push/pull state.

```text
┌ worktrees ──────────────────────────────────────────────────────────────────────────────────────────┐
│ > kees/                                                         3/7                                 │
│   WORKTREES    BRANCHES                                                                             │
│   branch            worktree        pr        review    threads  conflict  when   changes  sync     │
│ ▸ kees/parser-fix   parser-fix      #1234     approved  2                  2h     +3 ~1     ↑2      │
│   main              herdr-worktrees —         —         —                  4h     clean     ↓1      │
│   kees/queue-retry  —               #1235     —         —                  1d     —         merged  │
│   kees/old-spike    —               —         —         —                  3w     —         local   │
│ enter switch/create · ctrl-n new · alt+enter base… · ctrl-u update · ctrl-p open PR · GitHub: now   │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## Requirements

- Herdr 0.7.4+, macOS or Linux
- `git`, `fzf` 0.71+, Rust 1.87+ (`cargo`)
- Optional: [GitHub CLI](https://cli.github.com/) (`gh`) for pull request data

## Install

```bash
herdr plugin install SpaceK33z/herdr-worktrees
```

Add actions to `~/.config/herdr/config.toml`:

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

Then:

```bash
herdr config check
herdr server reload-config
```

## Key bindings

| Key | Action |
| --- | --- |
| `prefix+w` | Open the picker. |
| `enter` | Switch to a worktree, check out a local/remote-only branch or PR, or create the typed query via its create row. |
| `ctrl-n` | Create the typed query directly. |
| `alt+enter` / `prefix+shift+w` | Pick a custom base branch, then create. |
| `prefix+d` | Open the removal picker (`tab`/`shift-tab` to multi-select). |
| `ctrl-u` / `alt+u` | Merge or rebase the base into the highlighted worktree; on conflicts, hands the merge to a coding agent. |
| `ctrl-p` | Open the highlighted branch's pull request. |
| `ctrl-d` | Remove the highlighted worktree. |
| `ctrl-r` | Recompute metadata, bypassing the GitHub cache. |
| `ctrl-f` | Run `git fetch origin`, then recompute everything. |
| `esc` | Close the popup. |

## The picker

Rows are ordered: checked-out worktrees, then local branches without one, then
remote-only `origin/<branch>` rows. Selecting a remote row creates local
`<branch>` tracking `origin/<branch>`. With a nonempty query a **create
worktree** row appears last; typing a PR reference (`123`, `#123`, `pr:123`)
shows a **checkout pull request** row first instead.

Columns: **worktree** (directory name), **pr**, **review** (`approved`,
`changes`, `review`, `draft`), **threads** (unresolved review threads),
**conflict**, **when** (age), **changes** (`+staged ~unstaged` or `clean`),
and **sync** (`↑N` push, `↓N` pull, `—` synced, `local`, `remote`, `gone`,
`merged`). A `…` means the background scan is still running.

PR columns require a GitHub remote and an authenticated `gh`; set
`github-prs = false` to disable them. Results are cached for 60 seconds;
`ctrl-r`/`ctrl-f` always refetch.

## Creating a worktree

Type a name and pick the create row (or press `ctrl-n`). The plugin applies
`branch-prefix`, resolves `worktree-path`, fetches the remote base if
`fetch-before-create = true`, runs `git worktree add` from the configured base
(remote HEAD → `main`/`master` → current branch as fallback), opens it in Herdr,
copies `.worktreeinclude` entries, and runs `[pre-start].setup-worktree`. A
failing setup script leaves the worktree in place and reports the output.

### `.worktreeinclude`

A fresh worktree lacks gitignored files like `.env` or `node_modules/`. Add a
`.worktreeinclude` file at the repo root (`.gitignore` syntax) naming what to
carry over; entries are copied only when git also ignores them, before the setup
script runs. Copies are reflinked where supported. Read by Claude Code and
Worktrunk too. Disable with `worktree-include = false`.

### Pull request checkout

Typing a PR number checks out its head branch into a worktree. Same-repo PRs
create a normal tracking worktree; fork PRs fetch `refs/pull/N/head` into a new
local branch after confirmation. Independent of `github-prs`; disable with
`pr-checkout = false`.

### Command line

For scripts and coding agents (never run raw `git worktree add`):

```bash
herdr-worktrees create parser-fix          # prints the worktree path
herdr-worktrees create parser-fix --json   # {path, branch, base, action, setup}
```

Flags: `--exact` (skip prefix), `--base <ref>`, `--no-setup`.

## Updating a worktree

`ctrl-u` brings the base branch into a worktree with `--autostash`
(merge or rebase per `[update].strategy`). Clean updates just report and redraw.
On conflicts the plugin starts a coding agent via `herdr agent start` with a
task describing the merge; with the default `agent = "ask"` an fzf prompt picks
the agent first. Also works on worktrees already stopped mid-merge/rebase.

## Removing worktrees

`prefix+d` lists all worktrees except the main checkout, each with a safety
verdict after an untracked-file scan: green `✓ safe`, red `⚠ dirty`, yellow
`⚠ unpublished` (branch has unpushed commits) or `⚠ detached`. Multi-select with
`tab`, confirm with `enter`. Warned rows need confirmation unless
`[remove].force = true`. Local branches are kept unless `delete-branch = true`.
With it on, a branch is deleted without confirmation when its content is
already on the base branch — fully pushed, or landed via squash merge/rebase
(detected by content probes up to a simulated `git merge-tree`). A branch
still checked out in another worktree is never deleted.

## Configuration

```bash
$EDITOR "$(herdr plugin config-dir worktrees)/config.toml"
```

```toml
worktree-path = "{{ repo_path }}/.worktrees/{{ branch | sanitize }}"
base-branch = "main"          # fallback: remote HEAD, main/master, current branch
branch-prefix = ""            # e.g. "kees/" or "{{ user }}/"
open-mode = "workspace"       # "workspace" or "tab"
github-prs = true             # PR, review threads, conflict, merged state
auto-detect = true            # infer unset settings from the repo
pr-checkout = true            # checkout a PR by typing its number
worktree-include = true       # copy .worktreeinclude entries
fetch-before-create = true    # refresh origin/<base> before creating
show-worktree-name = true     # show the worktree directory name column

[popup]
width = "90%"
height = "70%"

[remove]
delete-branch = false
force = false                 # skip the dirty/unpublished warning

[update]
strategy = "merge"            # "merge" or "rebase"
agent = "ask"                 # a Herdr agent kind, or "ask" to choose each time
agents = ["claude", "codex", "opencode", "pi"]

[pre-start]
setup-worktree = '''
# runs in the new worktree; receives WORKTREE_PATH, WORKTREE_BRANCH,
# REPO_PATH, and BASE_BRANCH
'''
```

`worktree-path` supports `{{ repo_path }}`, `{{ repo_name }}`, `{{ branch }}`,
`{{ branch_short }}`, `{{ base }}`, and `{{ user }}`; the `sanitize` filter
replaces unsafe characters with `-`. `{{ user }}` is the git `user.name`
(already sanitized, so "Kees Kluskens" becomes `Kees-Kluskens`), falling back
to `$USER`. Config is read on every invocation. The
shape follows `~/.config/worktrunk/config.toml`.

### Per-repository settings

Any top-level setting can be overridden per repository, keyed by remote
(`host/owner/repo`, `owner/repo`, or repo name) or main-worktree path; most
specific wins:

```toml
[projects."github.com/acme/monolith"]
worktree-path = "/scratch/monolith/{{ branch | sanitize }}"
base-branch = "develop"
```

### Auto-detection

With no `worktree-path` set, the plugin adopts the layout the repo already uses,
from (most authoritative first): Worktrunk, gwq, phantom, ccmanager configs, the
layout of existing worktrees, or an ignored `.worktrees/`-style directory.
Foreign templates are only adopted when reproducible exactly.
`auto-detect = false` turns this off. Inspect with:

```bash
"$(herdr plugin dir worktrees)/target/release/herdr-worktrees" detect
```

## Troubleshooting

```bash
herdr plugin list --plugin worktrees --json   # manifest loaded?
herdr plugin log list --plugin worktrees --limit 20
```

Empty PR columns: run `gh auth status`, check for a GitHub remote, and try
`ctrl-r` (long-merged branches are filled in bounded batches). Stale push/pull
counts: run `git fetch` or press `ctrl-f`.

## Development

```bash
cargo build --release
herdr plugin link "$PWD"      # rebuild + re-link after changes: ./scripts/dev-relink.sh
```

Before opening a PR: `cargo fmt --all -- --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --locked`, `cargo package --locked`.
See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Herdr plugins are not sandboxed. This plugin runs `git`, `fzf`, `gh`, `herdr`,
and your setup script with your user permissions. Report security issues
privately through the repository's GitHub security advisory page.

## License

[MIT](LICENSE) © Kees Kluskens
