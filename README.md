# herdr-worktrees

**Your branches, PRs, and worktrees. One fuzzy picker.**

A [Herdr](https://herdr.dev) plugin to jump between tasks, spin up a worktree,
and review a PR without leaving your terminal.

```text
  WORKTREES    BRANCHES
  > _

    branch           worktree     pr     review    changes  sync
  ▸ kees/parser-fix  parser-fix   #1234  approved  +3 ~1    ↑2
    main             my-project  —      —         clean    ↓1
    kees/queue-retry —           #1235  review    —        remote

  enter switch/create · ctrl-n new · ctrl-u update · ctrl-p open PR
```

- **Jump straight in.** Fuzzy-find worktrees, local branches, and remote branches.
- **Start a task in seconds.** Type a branch name to create a worktree, or `#123` to check out a PR.
- **See what needs attention.** PR reviews, unresolved threads, conflicts, and unpushed commits at a glance.
- **Bring your environment along.** Copy ignored files with `.worktreeinclude` and run a setup script.
- **Keep work moving.** Merge or rebase from your base branch; hand conflicts to a coding agent.
- **Clean up with guardrails.** Get warnings for dirty worktrees and unpublished commits before removal.

## Quick start

**Requires:** Herdr 0.7.4+ on macOS or Linux, `git`, `fzf` 0.71+, and Rust 1.87+ (`cargo`).
For GitHub PR features, also install and authenticate [GitHub CLI](https://cli.github.com/) (`gh`).

### 1. Install

```bash
herdr plugin install SpaceK33z/herdr-worktrees
```

### 2. Bind a key

Add to `~/.config/herdr/config.toml` (merge into `[keys]` if it already exists):

```toml
[keys]
new_worktree = ""             # remove Herdr's built-in prefix+shift+g binding

[[keys.command]]
key = "prefix+w"
type = "plugin_action"
command = "worktrees.open"
description = "worktree: switch or create"
```

```bash
herdr config check
herdr server reload-config
```

### 3. Pick your next task

Open a Git repo in Herdr, then press your Herdr prefix key followed by `w`.

| Want to… | Do this in the picker |
| --- | --- |
| Switch to existing work | Search, select a row, press `enter` |
| Create a worktree | Type a new branch name, press `ctrl-n` |
| Check out a pull request | Type `#123`, select the PR row, press `enter` |
| Choose a different base | Press `alt+enter` |
| Update from the base branch | Press `ctrl-u` |
| Open a PR in your browser | Press `ctrl-p` |
| Remove a worktree | Press `ctrl-d` |

## Better together with Portboard

Pair with [Portboard](https://github.com/SpaceK33z/portboard) on Linux to keep
each worktree's dev servers, ports, and URLs straight:

- **herdr-worktrees:** create a worktree or jump into an existing one.
- **Portboard:** find its running servers, start or stop targets, and open the right URL—from a Herdr popup, CLI, or local dashboard.

## Make it yours

The plugin can detect your existing worktree layout. Customize paths, branch
prefixes, setup scripts, and whether worktrees open as workspaces or tabs.

| Guide | What's inside |
| --- | --- |
| [Configuration](docs/reference.md#configuration) | Options, per-repo settings, automatic layout detection |
| [Key bindings](docs/reference.md#key-bindings) | All shortcuts, refresh/fetch, bulk removal |
| [Creating worktrees](docs/reference.md#creating-a-worktree) | Custom bases, `.worktreeinclude`, fork PRs |
| [CLI & coding agents](docs/reference.md#command-line) | Script creation, JSON output, reuse the returned agent pane |
| [Updating & removing](docs/reference.md#updating-a-worktree) | Conflict handoff, safety checks, branch cleanup |
| [Troubleshooting](docs/reference.md#troubleshooting) | Logs, missing PR data, stale sync counts |

## Contributing

```bash
cargo build --release
herdr plugin link "$PWD"      # after changes: ./scripts/dev-relink.sh
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and checks before opening a PR.

## License

[MIT](LICENSE) © Kees Kluskens
