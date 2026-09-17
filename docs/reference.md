# Worktrees reference

[← Back to the README](../README.md)

- [Key bindings](#key-bindings)
- [The picker](#the-picker)
- [Creating a worktree](#creating-a-worktree)
- [Command line and coding agents](#command-line)
- [Updating a worktree](#updating-a-worktree)
- [Removing worktrees](#removing-worktrees)
- [Configuration](#configuration)
- [Troubleshooting](#troubleshooting)

## Key bindings

`prefix+w` uses the binding from the [quick start](../README.md#quick-start).
To also bind custom-base creation and the removal picker, add these to
`~/.config/herdr/config.toml` and reload with `herdr server reload-config`:

```toml
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

Symlinks below the destination checkout root are never followed when copying.
Source group and supported ACLs/security metadata are preserved before permissions
are applied; failures are reported rather than silently widening access.
Hard-linked files become independent copies, not shared inodes.

### Pull request checkout

Typing a PR number checks out its head branch into a worktree. Same-repo PRs
create a normal tracking worktree; fork PRs fetch `refs/pull/N/head` into a new
local branch after confirmation. Independent of `github-prs`; disable with
`pr-checkout = false`.

### Command line

For scripts and coding agents (never run raw `git worktree add`):

```bash
herdr-worktrees create parser-fix          # prints a summary with the worktree path
herdr-worktrees create parser-fix --json   # includes attachedWorkspaceId and rootPaneId
```

Flags: `--exact` (skip prefix), `--base <ref>`, `--no-setup`, `--no-open`.

When the repo is already open as a Herdr workspace, the created checkout is
attached there (unfocused) per the configured `open-mode` — its own worktree
space, or a tab inside the repo's workspace — exactly like creating through
the popup does. **Reuse that attachment when launching an agent; do not create
another workspace, tab, or split for the same checkout.**

On success, `--json` writes one JSON object to stdout; Git/include progress and
setup script output go to stderr. The object contains `path`, `branch`, `base`,
`action`, `setup`, `attachedWorkspaceId`, and `rootPaneId`. Start the requested
agent in the returned `rootPaneId`, which already has the checkout as its cwd:

```bash
# Run once, then read rootPaneId from the response:
herdr-worktrees create parser-fix --json
herdr agent start parser-fix --kind claude --pane <rootPaneId>
```

In workspace mode, `attachedWorkspaceId` identifies the new worktree workspace,
not the source repo workspace. In tab mode, it identifies the repo workspace
and `rootPaneId` identifies the new tab's pane. The legacy `workspace` field is
an alias for `attachedWorkspaceId` (older versions incorrectly returned the
source workspace).

Pass `--no-open` to skip attachment. IDs are `null` when attachment was skipped,
no source workspace was found, or attachment failed. Missing IDs are **not**
permission to create duplicate layout: inspect existing workspaces/tabs by
checkout path first. A setup failure also leaves the checkout and any attachment
in place; resolve the failure instead of rerunning creation.

## Updating a worktree

`ctrl-u` brings the base branch into a worktree with `--autostash`
(merge or rebase per `[update].strategy`). Clean updates just report and redraw.
On conflicts the plugin starts a coding agent via `herdr agent start` with a
task describing the merge; with the default `agent = "ask"` an fzf prompt picks
the agent first. Also works on worktrees already stopped mid-merge/rebase.
Conflicts while restoring an autostash are handled separately: no merge/rebase
continuation is attempted when that operation has already finished, and the
retained stash is left alone until the restored work is verified.

## Removing worktrees

`prefix+d` lists all worktrees except the main checkout, each with a safety
verdict after an untracked-file scan: green `✓ safe`, red `⚠ dirty`, yellow
`⚠ unpublished` (branch has unpushed commits) or `⚠ detached`. Multi-select with
`tab`, confirm with `enter`. Warned rows need confirmation unless
`[remove].force = true`. Local branches are kept unless `delete-branch = true`.

From a script, `herdr-worktrees remove --target <branch> <path> --yes` (or `-y`)
skips the prompt so no terminal is needed. The path may be spelled any way that
reaches the worktree — relative to the current directory, or through a
symlinked parent — as long as it is a registered worktree. It still refuses a
dirty, unpublished or detached worktree unless `--force` (`-f`) is also passed,
which is the command-line form of `[remove].force`. The removal itself runs
detached, as it does from the popup.
With it on, a branch is deleted without confirmation when its content is
already on the base branch — fully pushed, or landed via squash merge/rebase
(detected by content probes up to a simulated `git merge-tree`). A branch
still checked out in another worktree is kept.

During branch deletion, a temporary locked reservation worktree blocks ordinary
sibling checkout/worktree-add operations until the branch and its supporting refs
have been verified and deletion finishes. Do not concurrently switch or rewrite
the checkout being removed, or bypass Git's occupancy checks with force flags or
low-level ref/HEAD commands. Failed reservation cleanup reports its path and a
recovery command; it is not silently discarded.

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
