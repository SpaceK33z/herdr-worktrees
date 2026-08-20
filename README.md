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
│ enter switch/create · ctrl-n new · alt+enter base… · ctrl-u update · ctrl-p open PR · GitHub: now   │
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
| `ctrl-u` | Bring the base branch into the highlighted worktree, starting an agent if it conflicts. |
| `alt+u` | Choose a base branch, then update the highlighted worktree with it. |
| `ctrl-p` | Open the highlighted branch's pull request. |
| `ctrl-d` | Remove the highlighted worktree from the main picker. |
| `ctrl-r` | Recompute local and remote-tracking metadata, bypassing the GitHub cache. |
| `ctrl-f` | Run `git fetch origin`, then recompute everything against the updated remote-tracking refs. |
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
seconds: opening the picker reuses a cached entry that is still fresh, while
`ctrl-r` and `ctrl-f` always fetch. The dim footer timestamp shows when GitHub
data was last fetched, and reads `GitHub: failed` when the last fetch could not
reach GitHub. Pull counts use local remote-tracking refs; press `ctrl-f` when
you need current remote state.

## Creating a worktree

Type a branch name and select the **create worktree** row to create that exact
query even when existing entries fuzzy-match it. `ctrl-n` remains a direct
shortcut for the same operation. The plugin then:

1. Applies `branch-prefix` and resolves `worktree-path` (detected from the repo
   when unset — see [Auto-detection](#auto-detection)).
2. Refreshes the base when it is a remote-tracking branch, so the new branch
   starts from the current upstream tip (see [Fetching the
   base](#fetching-the-base)).
3. Runs `git worktree add` from the configured base, the remote HEAD, `main` or
   `master`, or the current branch (in that order).
4. Opens the checkout in Herdr as a nested workspace or tab, depending on
   `open-mode`.
5. Copies the files named by `.worktreeinclude`, then runs
   `[pre-start].setup-worktree` asynchronously in the new checkout.

For an existing remote-only row, the remote branch name is used as-is: the
plugin does not apply `branch-prefix`. For example, selecting `origin/topic`
creates local `topic`, while `origin/kees/topic` creates local `kees/topic` and
keeps the usual prefix-aware `branch_short` path behavior.

The setup script receives `WORKTREE_PATH`, `WORKTREE_BRANCH`, `REPO_PATH`, and
`BASE_BRANCH`. A non-zero exit leaves the worktree in place and reports the
script output instead of silently continuing.

### Fetching the base

New branches are created from `origin/<base>`, which is only as current as the
last fetch. Before creating one, the plugin runs a targeted
`git fetch <remote> <base>` so the branch starts from the upstream tip:

```toml
fetch-before-create = true    # refresh origin/<base> before creating a branch
```

- The fetch is best effort. If the remote is unreachable, rejects the fetch, or
  takes longer than 10 seconds, the plugin says so and creates the branch from
  the local copy anyway.
- Only the base branch is fetched, without tags — not the whole remote.
- It is skipped when the base is a local branch, when the branch already exists
  locally (nothing is created from the base then), and for `--dry-run`.
- Set `fetch-before-create = false` to keep creation entirely offline.

Checking out an existing remote branch or pull request always fetches that ref,
independent of this setting.

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

## Carrying gitignored files over

A worktree is a fresh checkout, so gitignored files a project needs to run —
`.env`, a local secrets file, a warm dependency directory — are not in it. Add a
`.worktreeinclude` file to the repo root, in `.gitignore` syntax, naming what to
carry over:

```text
.env
.env.local
config/secrets.json
node_modules/
```

An entry is copied only when it is **both** named by that file **and** ignored by
git, so tracked files are never duplicated. Without the file nothing is copied.
The same convention is read by [Claude Code](https://code.claude.com/docs/en/worktrees)
and [Worktrunk](https://worktrunk.dev), so one file serves all
three.

- Files land before `[pre-start].setup-worktree` runs, so the script can rely on
  a copied `.env`.
- git does the matching, so anchoring, `**`, and negation work as they do in
  `.gitignore`.
- Existing files in the new worktree are left alone; a directory that holds
  another checkout is skipped.
- Copies are reflinked where the filesystem supports it (APFS, Btrfs), so
  carrying a large dependency directory over costs neither time nor disk until
  something in it is written.
- Set `worktree-include = false` to ignore the file, globally or per repository.

To see what a new worktree would receive:

```bash
"$(herdr plugin dir worktrees)/target/release/herdr-worktrees" include
```

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

## Updating a worktree

Press `ctrl-u` on a worktree row to bring the base branch into it. The base is
fetched first, then merged (or rebased, see `[update].strategy`) with
`--autostash`, so uncommitted work does not block the update and is restored
afterwards. Press `alt+u` instead to pick the base branch for this update.

Nothing needs a human when the update is clean: the picker reports `merged
origin/main into kees/parser-fix` and redraws with the new sync state. A
worktree that already has the base says so and does nothing.

When git stops on conflicts, the plugin starts a coding agent in that worktree
through `herdr agent start` and hands it a one-line task describing the merge,
the conflicted files, and how to finish. With the default `agent = "ask"` a
small fzf prompt asks which agent to use first; `esc` skips it and leaves the
conflict for you. A worktree that is *already* stopped mid-merge or mid-rebase
takes the same path, so `ctrl-u` also works as "hand this conflict to an agent".

```toml
[update]
strategy = "merge"            # "merge" or "rebase"
agent = "ask"                 # a Herdr agent kind, or "ask" to choose each time
agents = ["claude", "codex", "opencode", "pi"]   # what "ask" offers
prompt = ""                   # override the task the agent is started with

[update.agent-args]
claude = ["--permission-mode", "acceptEdits"]    # extra argv per agent kind
```

`prompt` is rendered with `{{ branch }}`, `{{ base }}`, `{{ strategy }}`,
`{{ past }}` (`merged`/`rebased`), `{{ continue }}` (`merge --continue` or
`rebase --continue`), and `{{ files }}`. Keep it to one line — Herdr submits the
prompt it sends, so a newline would submit the first line on its own.

Like every other setting, `[update]` can be set per repository:

```toml
[projects."github.com/acme/monolith".update]
strategy = "rebase"
agent = "codex"
```

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

Warned rows show what makes them unsafe unless `[remove].force = true`; enter
confirms either way. Local branches are kept unless `delete-branch = true`.

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
auto-detect = true            # infer unset settings from the repo (see below)
pr-checkout = true            # checkout a PR by typing its number
worktree-include = true       # copy .worktreeinclude entries into new worktrees
fetch-before-create = true    # refresh origin/<base> before creating a branch
show-worktree-name = true     # show the worktree directory name column

[popup]
width = "90%"
height = "70%"

[remove]
delete-branch = false
force = false                 # skip the dirty and unpublished warning

[update]
strategy = "merge"            # "merge" or "rebase" the base into a worktree
agent = "ask"                 # a Herdr agent kind, or "ask" to choose each time

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

### Per-repository settings

Any top-level setting can be overridden per repository. Keys match the remote
(`host/owner/repo`, `owner/repo`, or just the repo name) or the absolute path of
the main worktree; the most specific matching key wins.

```toml
worktree-path = "{{ repo_path }}/.worktrees/{{ branch | sanitize }}"

[projects."github.com/acme/monolith"]
worktree-path = "/scratch/monolith/{{ branch | sanitize }}"
base-branch = "develop"

[projects."/home/dev/experiments"]
branch-prefix = ""
```

### Auto-detection

With no `worktree-path` set, the plugin looks for a layout the repo has already
declared, so worktrees land where the rest of your tooling puts them. Sources,
most authoritative first:

| Source | Read from |
| --- | --- |
| `worktrunk` | `$WORKTRUNK_WORKTREE_PATH`, `<repo>/.config/wt.toml`, `~/.config/worktrunk/config.toml` (including its `[projects."…"]` tables) |
| `gwq` | `<repo>/.gwq.toml`, `~/.config/gwq/config.toml` (`[[repository_settings]]`, `worktree.basedir`, `naming.template`) |
| `phantom` | `git config phantom.worktreesDirectory`, `<repo>/phantom.config.json` |
| `ccmanager` | `~/.config/ccmanager/config.json` |
| `observed` | the layout the repo's existing worktrees already follow |
| `gitignore` | an ignored `.worktrees/`, `worktrees/`, `.wt/` or `.claude/worktrees/` |

Foreign templates are only adopted when they can be reproduced exactly; one that
uses a variable or filter this plugin cannot render is skipped rather than
approximated. Nothing is detected when `worktree-path` is set, and
`auto-detect = false` turns it off entirely.

To see what was found and what won:

```bash
"$(herdr plugin dir worktrees)/target/release/herdr-worktrees" detect
```

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
unpublished warnings reduce accidental deletion; `force = true` hides them.

Report security issues privately through the repository's GitHub security
advisory page rather than a public issue.

## License

[MIT](LICENSE) © Kees Kluskens
