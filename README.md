# herdr-worktrees

A [Herdr](https://herdr.dev) plugin for working with git worktrees from a popup.
The fuzzy list shows active worktrees first, followed by local branches that are
not checked out. Each row includes its PR and review state, age, merge status,
and, for worktrees, uncommitted changes.

```
┌ worktrees ────────────────────────────────────────────────────────────────┐
│ > kees/                                                         3/7       │
│   branch            pr        review    conflict  when   changes  status  │
│   WORKTREES                                                             │
│ ▸ kees/parser-fix   #1234     approved           2h     +3 ~1     ↑2      │
│   main              —         —                  4h     clean     —       │
│   BRANCHES                                                              │
│   kees/queue-retry  #1235     review             1d     —         squashed│
│   kees/old-spike    —         —                  3w     —         merged  │
│ enter switch/create · alt+enter base… · ctrl-p open PR · ctrl-d delete · ctrl-r refresh · esc close │
└────────────────────────────────────────────────────────────────────────────┘
```

## Keys

| Key | What happens |
| --- | --- |
| `prefix+w` | Open the worktree popup. Type to filter immediately. |
| `enter` | Switch to the highlighted worktree, or check out the highlighted branch in a new worktree. If your query matches nothing, create a branch off the base branch (`main` by default). |
| `alt+enter` | Create off a **custom base**: pick a base branch in a second picker, then create. |
| `prefix+shift+w` | Open the popup already in custom-base mode. |
| `prefix+d` | Open the delete popup. Filter, `enter` to remove the worktree. |
| `ctrl-p` | Open the highlighted branch's pull request in your browser. |
| `ctrl-d` | Delete the highlighted worktree in place (from the switch/create picker). |
| `ctrl-r` | Recompute the list (merge status is cached per commit). |
| `esc` | Close the popup, layout untouched. |

## The list

The popup has two sections: checked-out **worktrees** first, then local
**branches** without a worktree. Selecting a branch creates its worktree;
selecting a worktree switches to it. Rows are computed in parallel from plain
git:

- **pr** — the PR number for the branch's head, as a clickable link
  (ctrl-click opens it). `—` when there is no PR.
- **review** — `approved`, `changes` (changes requested), `review` (review
  required), or `draft`. Draft PRs are marked clearly even when GitHub reports
  them as review-required. `—` when the branch has no open PR (merged PRs show
  their state in the `status` column instead).
- **conflict** — a red `conflict` marker when the PR has merge conflicts;
  blank otherwise.
- **when** — committer date of `HEAD`, relative.
- **changes** — `+staged ~unstaged` from `git status --porcelain
  --untracked-files=no`, or `clean`. Untracked files are not enumerated in the
  list (that walk dominates on huge repos); deletion re-checks with a full
  status, so it still refuses untracked-only changes.
- **status** — one of:
  - `merged` — `git merge-base --is-ancestor <branch> <base>` (or the PR was
    merged on GitHub). A branch with an unmerged PR and no unique commits shows
    `—`, or `↓N` if the base has advanced.
  - `squashed` — the branch's tree, replayed onto the merge base as a synthetic
    commit, is patch-identical to something already on the base
    (`git commit-tree` + `git cherry`). This is what catches squash-merged and
    rebase-merged PRs, which the ancestor check misses.
  - `↑N` / `↓N` — unmerged work relative to the base branch: `↑N` commits are on
    the branch but not the base (ahead), `↓N` commits are on the base but not
    the branch (behind). Both together (`↑2 ↓1`) means the branch and base have
    diverged.
  - `—` — the base branch itself.

The `pr` / `review` / `conflict` columns come from `gh pr list`, so they need
`github-prs = true` (and a GitHub remote). The lookup runs in the background
with a 1.5s timeout and is cached for 60s, so it never blocks the list; the
local merge detection above remains the fallback.

## Creating

Typing a branch name that matches no existing worktree turns `enter` into a
create. The plugin:

1. Applies `branch-prefix`, then resolves the path from `worktree-path`.
2. `git worktree add` off the base branch (`main`, or whatever `alt+enter`
   selected).
3. Runs the `[pre-start] setup-worktree` script in the new checkout — this is
   where you install deps, copy `.env` files, or boot services. It inherits
   `WORKTREE_PATH`, `WORKTREE_BRANCH`, `REPO_PATH` and `BASE_BRANCH`, and runs
   with the new worktree as cwd. A non-zero exit leaves the worktree in place and
   surfaces the output in the popup instead of silently continuing.
4. Registers the checkout with `herdr worktree open`, so it shows up as a nested
   worktree workspace in the sidebar (or a tab, with `open-mode = "tab"`).

### Branch prefixes

Set `branch-prefix` and every branch you create gets it, so you type
`parser-fix` and land on `kees/parser-fix`:

```toml
branch-prefix = "kees/"
branch-prefix = "{{ user }}/"     # from git config user.name, falling back to $USER
branch-prefix = "u/{{ user }}/"
```

- The prefix is skipped if what you typed already starts with it, so `kees/foo`
  stays `kees/foo` rather than becoming `kees/kees/foo`.
- A leading `/` opts out for one branch: `/hotfix-ci` creates `hotfix-ci`.
- Filtering is unaffected — typing `parser` still matches `kees/parser-fix`, and
  the prefix renders dimmed in the list so the part you care about stands out.
- `worktree-path` sees both `{{ branch }}` (`kees/parser-fix`) and
  `{{ branch_short }}` (`parser-fix`), so you choose whether the prefix shows up
  in the directory name.

## Deleting

`prefix+d` lists everything except the main checkout, with the same columns — so
you can see at a glance which branches are safe to drop. `enter` asks for
confirmation, then removes the worktree and closes its Herdr workspace and panes.
Dirty worktrees and unmerged branches are refused unless you confirm with
`ctrl-x`. The branch itself survives unless `delete-branch = true`.

## Configuration

Same shape as `~/.config/worktrunk/config.toml`, so you can copy yours over:

```bash
$EDITOR "$(herdr plugin config-dir worktrees)/config.toml"
```

```toml
worktree-path = "{{ repo_path }}/.worktrees/{{ branch | sanitize }}"
base-branch = "main"          # fallback: the remote HEAD, then the current branch
branch-prefix = ""            # e.g. "kees/" — prepended to branches you create
open-mode = "workspace"       # "workspace" (nested in the sidebar) or "tab"
github-prs = false            # cross-check merge status with `gh`

[popup]
width = "90%"
height = "70%"

[remove]
delete-branch = false
force = false                 # skip the dirty/unmerged guard

[pre-start]
setup-worktree = '''
primary="$(git worktree list --porcelain | awk '/^worktree / { sub(/^worktree /, ""); print; exit }')"
setup="$primary/scripts/setup-worktree.sh"

if [ -f "$setup" ]; then
  bash "$setup" "$PWD"
fi
'''
```

Template variables in `worktree-path`: `{{ repo_path }}`, `{{ repo_name }}`,
`{{ branch }}`, `{{ branch_short }}`, `{{ base }}`, `{{ user }}`. The `sanitize`
filter replaces `/` and other unsafe characters with `-`. The file is read on every invocation — no reload needed.

## Install

```bash
herdr plugin install SpaceK33z/herdr-worktrees
```

Then bind it in `~/.config/herdr/config.toml`:

```toml
[keys]
new_worktree = ""             # drop Herdr's built-in prefix+shift+g

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

```bash
herdr config check && herdr server reload-config
```

Requires Herdr ≥ 0.7.4, `git`, and `fzf`. macOS and Linux. A Rust toolchain
(`cargo`) to build from source. `gh` only if you enable `github-prs`.

## Development

The plugin is a single Rust binary. Build and link the checkout instead of
installing it:

```bash
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke open --plugin worktrees
```

Rebuild after edits (`plugin link` does not run the `[[build]]` step), and
re-link when the manifest changes (Herdr caches it):

```bash
cargo build --release
./scripts/dev-relink.sh        # build + unlink + link + action list
```

Iterate on the list rendering outside Herdr:

```bash
cargo run --release -- --json | jq
cargo run --release -- --fzf
cargo run --release -- picker --dry-run demo-branch   # print instead of acting
cargo test                     # integration tests (git metadata engine)
cargo clippy
herdr plugin log list --plugin worktrees --limit 20
```

Layout:

```
herdr-plugin.toml     actions + popup panes
src/config.rs         plugin config parsing, path templating
src/model.rs          metadata engine: commit, age, changes, merge status
src/status.rs         merged/squashed/ahead-behind detection + cache
src/render.rs         fzf column rendering
src/picker.rs         prefix+w  — switch / create
src/remove.rs         prefix+d  — delete
src/open.rs           opens the popup pane for an action
src/setup.rs          runs [pre-start] setup-worktree
tests/engine.rs       integration tests
```

## License

MIT © Kees Kluskens
