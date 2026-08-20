# Bug Audit — herdr-worktrees

Audit date: 2026-08-19. All 17 source files read in full; suspicious paths traced end-to-end. Two findings were verified empirically (a probe unit test for the theme panic; pty-driven fzf runs for abort semantics). `cargo test` (47 passed) and `cargo clippy --all-targets` are clean. The destructive remove flow is notably well-guarded (re-inspection before deletion, HEAD/OID verification, update-ref transactions), so no critical data-loss bugs were found.

## `git worktree add` failures are swallowed while the error message says "see above"

- **File**: src/picker.rs:946, src/picker.rs:951, src/picker.rs:911
- **Severity**: medium
- **Description**: `create_worktree` and `add_remote_tracking_worktree` run `git worktree add` via `git::git_success`, which uses `Command::output()` — capturing and discarding both stdout and stderr (src/git.rs:21-25). On failure the user sees `"git worktree add failed (see above)"`, but nothing was printed above because git's diagnostics were captured and thrown away. The PR-checkout paths (`checkout_same_repo_pr`, `checkout_fork_pr`) correctly use `git_inherit` instead.

  ```rust
  if !git::git_success(&["worktree", "add", path.as_str(), final_branch.as_str()]) {
      tty::err("git worktree add failed (see above)");
  ```

  Additionally, these two calls in `create_worktree` are the only `worktree add` invocations that omit `-C repo`, so they run in the popup pane's cwd. With a relative `worktree-path` template (e.g. `.worktrees/{{ branch | sanitize }}`), the path resolves against whatever worktree the popup was opened from, while the remote/PR checkout paths (which pass `-C repo`) resolve it against the repo root — the same config puts worktrees in different places depending on which code path created them.
- **Failure scenario**: Type a new branch name and press Enter while the target directory already exists or the branch is checked out elsewhere. The popup shows only "git worktree add failed (see above)" with no explanation.
- **Suggested fix**: Use `git_inherit` (as the PR paths do) and pass `-C repo` consistently.

## `status_counts` misses unmerged (`U`) and typechange (`T`) codes — conflicted worktrees report "clean"

- **File**: src/model.rs:700-707
- **Severity**: medium
- **Description**: The porcelain parser only counts `M|A|D|R|C` as staged and `M|D` as unstaged:

  ```rust
  if matches!(x, 'M' | 'A' | 'D' | 'R' | 'C') { staged += 1; }
  if matches!(y, 'M' | 'D') { unstaged += 1; }
  ```

  A worktree mid-merge with conflicts produces `UU` (both modified) or `UA` lines — neither column matches, so the picker renders it as `clean` / `dirty: false`. Typechange (`T`) is likewise missed in both positions. This also makes `worktree_dirty` / `worktree_dirty_checked` (src/model.rs:716-738) return `false` for a conflict-only worktree, despite doc comments calling them the "precise dirty check … at delete time for destructive actions". They are currently uncalled, but they are the public API a caller would reach for, and they are wrong in exactly the case that matters. The actual remove flow is safe: it uses its own `parse_changes` (src/remove.rs:672-690), which counts any non-space/non-`?` code as staged.
- **Failure scenario**: A worktree with an in-progress merge holding only `UU` conflict entries shows `clean` in the picker's changes column.
- **Suggested fix**: Count `x` in `U|T` as staged and `y` in `U|T` as unstaged (or mirror `parse_changes`'s logic), with a regression test for `UU`.

## Panic in `parse_color` on multibyte characters in a hex color — crashes the picker's reload helpers

- **File**: src/theme.rs:170-177
- **Severity**: medium
- **Description**: After `strip_prefix('#')`, the code checks `hex.len() == 6` (a **byte** length) and then byte-slices `&hex[0..2]`, `&hex[2..4]`, `&hex[4..6]`. A multibyte character across a slice boundary panics with "byte index is not a char boundary". Verified with a probe test: `parse_color("#aéxyz")` panics (`'a'` = 1 byte + `'é'` = 2 bytes puts index 2 mid-character). The value comes from the user's Herdr `config.toml` (`[theme.custom] accent/teal`). `ThemeColors::load()` runs inside `run_cached_list` and `run_cache_refresh` — the helpers fzf invokes on every keystroke and on load/ctrl-r — so the panic makes every reload return nothing and the picker renders an empty list with no diagnosis.
- **Failure scenario**: `accent = "#aé123"` in Herdr's config → picker list is permanently empty.
- **Suggested fix**: Validate `hex.chars().all(|c| c.is_ascii_hexdigit())` before slicing, or use `hex.get(0..2)?`.

## Same-repo PR checkout silently reuses a stale local branch

- **File**: src/picker.rs:850-853
- **Severity**: low
- **Description**: `checkout_same_repo_pr` short-circuits when a local branch matching the PR's head-ref name exists, without fetching or comparing it against `target.head_oid`. "Checkout pull request #N" can thus produce a worktree missing the PR's current commits, with no warning. The fork path (src/picker.rs:893-902) explicitly refuses a divergent local branch; the same-repo path has no equivalent check.
- **Failure scenario**: A teammate pushes new commits to PR #12's branch, which you also have locally from last week. Typing `12` + Enter creates a worktree at your week-old commit, presented as the PR checkout.
- **Suggested fix**: Compare the local branch OID with `target.head_oid`; warn or fast-forward on mismatch.

## Removal log path has one-second resolution — concurrent removals collide and cross-terminate progress panes

- **File**: src/background.rs:33-37, src/remove.rs:417
- **Severity**: low
- **Description**: `log_path` builds `<state>/logs/remove-<epoch-seconds>.log`, so two removals started within the same second share a path. `delete_worktrees` then does `std::fs::File::create(&log)`, truncating the first removal's live log, and both background workers append to one file. `run_progress` exits at the first `__HERDR_WORKTREE_REMOVE_DONE__` marker, so whichever batch finishes first closes both progress panes, and interleaved lines land in the wrong pane.
- **Failure scenario**: ctrl-d one worktree in the picker, then delete another via the remover within the same wall-clock second.
- **Suggested fix**: Include PID plus a nanosecond timestamp or counter in the filename (as `unique_cache_path` in picker.rs already does).

## Kill-by-PID race after subprocess timeout can signal an unrelated process

- **File**: src/pr.rs:67-77 (also 190-200 and 234-244)
- **Severity**: low
- **Description**: When `rx.recv_timeout` expires, the code SIGKILLs the saved raw PID while a separate waiter thread owns the `Child` and is concurrently in `wait_with_output()`. If the child exits between the timeout firing and the `kill()` call, the waiter reaps it, the PID becomes reusable, and the SIGKILL can hit a freshly spawned unrelated process. The `// SAFETY` comment asserts the child is "still-running", but nothing enforces that at kill time.
- **Failure scenario**: `gh pr view` completes at ~5.0s just after the timeout; its PID is recycled by another process that then receives SIGKILL. Rare, but the window is real.
- **Suggested fix**: Share the `Child` (e.g. `Mutex<Option<Child>>`) with the waiter and call `child.kill()`, which guards against already-reaped children, instead of raw `libc::kill` on a copied PID.

## Progress pane loops forever if the removal worker dies without unwinding

- **File**: src/remove.rs:768-793
- **Severity**: low
- **Description**: `run_progress` polls the log every 100ms until it sees `PROGRESS_DONE`, with no timeout and no liveness check. The `ProgressCompletion` drop guard covers normal exits and panics, but not SIGKILL or OOM-kill — then the marker is never written and the Herdr pane spins indefinitely until manually closed.
- **Failure scenario**: The detached `remove-bg-batch` process is killed while removing a large worktree; the "Removing…" pane never exits.
- **Suggested fix**: Record the worker PID in the log header and exit with an error line when that process no longer exists, or add a generous inactivity timeout.

---

## Verified non-issues (investigated because they looked like bugs)

- **Esc-after-typing does not create a worktree.** src/picker.rs:58 assumes an aborted fzf produces empty output; fzf 0.74.2 was driven through a pty with a real ESC keypress and confirmed stdout is empty on abort (exit 130) even with `--print-query`, so the empty-query close check is sound and the `else if !query.is_empty()` create branch at src/picker.rs:165 is unreachable via abort.
- The remove flow's TOCTOU handling is solid: targets are re-inspected in the background worker (`validate_and_delete`), HEAD is compared against the confirmed OID, new risks after confirmation are rejected, and branch deletion uses a `git update-ref --stdin` transaction verifying the upstream OID captured at publication-check time.
- Stale "…"/"loading" fields from the picker's fast initial frame cannot cause an unsafe ctrl-d delete — `delete_worktrees` ignores the row's cached kind/changes and re-inspects.
