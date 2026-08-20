# Improvements Audit — herdr-worktrees

Audit date: 2026-08-19. Findings ordered by impact.

## GitHub PR cache is written but never read in the actual picker flow
- **Area**: performance
- **File**: src/picker.rs:632 (with src/model.rs:109 and src/pr.rs:97)
- **Impact**: high
- **Description**: `run_cache_refresh` calls `model::compute_all(&repo, &config, &state_dir, false)` — `use_cache = false`. This command backs *both* the fzf `load` event (every picker open) and `ctrl-r`, so every picker open bypasses the 60-second PR cache and runs a fresh `gh pr list --state all --limit 1000` plus a GraphQL thread-count query (up to ~5s of `reload-sync` latency per open, worst case). The cache in pr.rs is written on every fetch but effectively never read: `PICKER_INITIAL` has `fetch_prs: false`, so the initial frame doesn't consult it either and always shows empty PR columns even when fresh cached data exists. The README's "GitHub results are cached for 60 seconds" describes behavior that doesn't happen in practice.
- **Suggestion**: Differentiate the two triggers: have the `load` binding invoke `picker-cache-refresh --cached` (use_cache = true) and keep `ctrl-r` cache-bypassing. Additionally, let `compute_picker_initial` hydrate PR columns from cache without spawning `gh` (read-only `cache_get`, no fetch), so a reopen within 60s shows PR data in the first frame.

## "git worktree add failed (see above)" — but there is nothing above
- **Area**: UX
- **File**: src/picker.rs:946-960 (also 779-783, 911-915)
- **Impact**: high
- **Description**: `create_worktree` and `add_remote_tracking_worktree` use `git::git_success`, which runs git via `.output()` — stdout/stderr are captured and *discarded*. The user then sees "git worktree add failed (see above)" with an empty pane above it. Common failures (invalid branch name, path already exists, branch checked out elsewhere) become undiagnosable. Relatedly, the PR-checkout paths (`checkout_same_repo_pr`, `checkout_fork_pr`) do use `git_inherit` so the git error *is* printed, but on worktree-add failure `checkout_pr_worktree` returns without `tty::wait_key()`, so the popup closes instantly and the error flashes by unread.
- **Suggestion**: For these explicit user actions, either switch to `git_inherit` (like the PR paths) or capture stderr and include it in the `tty::err` message; and add the `tty::err` + `wait_key` treatment to the PR-checkout worktree-add failure paths. While there, add `-C repo` to the `create_worktree` invocations for consistency — they are the only worktree-add calls relying on the process cwd.

## Destructive and trust confirmations auto-accept when stdin is not a TTY
- **Area**: robustness
- **File**: src/tty.rs:53-68
- **Impact**: high
- **Description**: `tty::confirm` treats `read_key() == None` as acceptance for non-force prompts, and `read_key` returns `None` both when stdin isn't a terminal *and* on any read error. That means `herdr plugin action invoke`, a broken pane, or any scripted invocation silently confirms worktree removal (`delete_worktrees`) and — more worryingly — the fork-PR trust prompt (`confirm_fork_checkout`), whose whole purpose is to stop untrusted code from reaching the setup script. The safe default for a guard is deny, not accept.
- **Suggestion**: Make `confirm` return `false` when stdin is not a terminal (check `is_terminal()` explicitly, separately from read errors), with a message explaining how to proceed interactively. If some flow genuinely needs headless confirmation, gate it behind an explicit flag rather than the absence of a TTY.

## PR checkout failure message blames `gh` auth for every failure mode
- **Area**: UX
- **File**: src/pr.rs:52-83, src/picker.rs:790-795
- **Impact**: medium
- **Description**: `resolve_pr` pipes stderr and its doc comment claims it "captures stderr for diagnostics", but the stderr is never read — on any failure the caller prints "could not resolve pull request #N (requires an authenticated gh CLI)". A typo'd PR number, a network timeout, a missing `gh` binary, and an auth problem all produce the same misleading message. This is a synchronous, explicit user action (unlike the background fetches), so there's no reason to be terse.
- **Suggestion**: Return a `Result` with distinguishable causes: spawn error → "gh is not installed"; timeout → "gh timed out after 5s"; non-zero exit → include the first line of gh's stderr (which already says things like "Could not resolve to a PullRequest with the number 999").

## Background GitHub failures are invisible; 1.5s timeout permanently starves slow networks
- **Area**: robustness
- **File**: src/pr.rs:190 (timeout), src/picker.rs:235-257 (footer)
- **Impact**: medium
- **Description**: When `gh pr list` fails or exceeds the hard-coded 1500ms timeout, `fetch_many` silently returns cached/empty data. The footer keeps showing the last successful refresh ("GitHub: 3d ago") or "not refreshed", so a user with a slow connection or expired auth sees empty PR columns forever with no hint why. On networks where `gh` reliably takes >1.5s, PR columns can *never* populate, even via a deliberate `ctrl-r`.
- **Suggestion**: Record the failure (e.g., alongside `last-refresh.json`) and render it in the footer ("GitHub: failed — check gh auth"). Give the explicit `ctrl-r` path a more generous timeout (the 5s used by `resolve_pr`) and consider a `github-timeout-ms` config key for the background path.

## Non-checked-out branch sync falls back to one serial `git rev-list` per branch
- **Area**: performance
- **File**: src/model.rs:303-318 (with src/status.rs `left_right_count`)
- **Impact**: medium
- **Description**: Worktree inspection is parallelized (scoped threads, 4×cores), but the `branches` list is computed serially on the main thread. For each local branch with no configured upstream but a same-named diverged `origin/` ref, `compute_sync` spawns `git rev-list --left-right --count` — one subprocess at a time. In a repo with dozens of such branches (common when branches are pushed without `--track`), the synchronous `reload-sync` refresh gains hundreds of milliseconds to seconds of avoidable serial latency.
- **Suggestion**: Reuse the same chunked `std::thread::scope` pattern for `branch_records` that worktrees already use, or (git ≥ 2.41) fold the computation into the existing `for-each-ref` call with ahead-behind formats where applicable.

## Removal progress pane spins forever if the worker dies; log names collide within one second
- **Area**: robustness
- **File**: src/remove.rs:753-794, src/background.rs:33-42
- **Impact**: medium
- **Description**: `run_progress` polls the log every 100ms until it sees the `__..._DONE__` marker. `ProgressCompletion`'s `Drop` covers panics, but a SIGKILLed or OOM-killed worker leaves the pane on "Preparing removal…" with a spinner forever, and the user cannot distinguish "slow removal" from "dead worker". Separately, `background::log_path` names logs `<kind>-<epoch-seconds>.log`: two removals started in the same second share one log file, so one worker's DONE marker terminates the other's progress pane mid-run and their lines interleave.
- **Suggestion**: Add a PID (and a counter/nanos) to the log filename. In `run_progress`, have the parent pass the worker PID (or write it as the log's first line) and exit with "removal worker exited unexpectedly — see <log>" when the PID is gone without a DONE marker; alternatively a no-progress watchdog (e.g., 60s without file growth).

## No branch-name validation before attempting creation
- **Area**: UX
- **File**: src/picker.rs:917-965
- **Impact**: medium
- **Description**: The create row accepts any typed query — `foo..bar`, `foo bar`, names ending in `/`, `.lock` suffixes — and the failure only surfaces at `git worktree add`, currently as the empty "(see above)" error. Since the create row is the picker's headline feature, a bad name should fail with a clear, immediate message.
- **Suggestion**: Before `git worktree add`, run `git check-ref-format --branch <final_branch>` (one cheap spawn on an explicit action) and report "'foo bar' is not a valid branch name" via `tty::err` + `wait_key`. This also protects the ctrl-n and alt-enter paths.

## Cheap win: a fetch keybinding for stale remote state
- **Area**: feature
- **File**: src/picker.rs:351 (bind string)
- **Impact**: medium
- **Description**: The README twice tells the user to "run `git fetch`" manually when pull counts look stale — which means leaving the picker, fetching, and reopening. The entire refresh plumbing (`refresh_cmd`, `reload-sync`, `transform-footer`) already exists; only the fetch itself is missing.
- **Suggestion**: Add e.g. `ctrl-f:reload-sync(<exe> picker-cache-fetch <cache> {q})+first+transform-footer(...)` where the new helper runs `git fetch --quiet origin` (with a timeout) before delegating to the existing `run_cache_refresh` logic. Document it in the footer and README key table.

## Test gaps: base-ref resolution, PR cache TTL, background-batch protocol
- **Area**: tests
- **File**: src/git.rs:118-160, src/pr.rs:406-449, src/remove.rs:816
- **Impact**: medium
- **Description**: Three pieces of important logic are untested: (1) `git::resolve_base_ref` — a near-duplicate of the tested `model::resolve_base_ref`, with its own precedence chain (configured base → origin/HEAD → main/master → current); the duplication invites divergence, and only one copy has coverage. (2) The PR cache lifecycle (`cache_get`/`cache_store`/TTL expiry/repo-branch validation) — only path hashing is tested; a TTL or serialization regression would ship silently. (3) The `remove-bg-batch` argv protocol: `delete_worktrees` encodes branch/path/head/risk quadruples that `run_background_batch` decodes; there is no round-trip test, so a field-order change on one side would only be caught by a live removal.
- **Suggestion**: Consolidate the two `resolve_base_ref` implementations (have git.rs delegate to the snapshot-based one) and test the precedence chain once; add a cache round-trip test with a mocked timestamp; extract the batch encode/decode into shared functions and add a round-trip test.

## State directory grows without bound
- **Area**: robustness
- **File**: src/pr.rs:419-449, src/background.rs:33-42
- **Impact**: low
- **Description**: Per-branch PR cache files (`pr-info-v2/<hash>/<hash>.json`) are created for every branch ever seen and never deleted — the 60s TTL invalidates entries but leaves files. Setup/removal logs (`logs/*.log`) likewise accumulate one file per operation forever. Under the default `/tmp/herdr-worktrees-state` this self-heals on reboot, but under a real `HERDR_PLUGIN_STATE_DIR` it's permanent growth.
- **Suggestion**: Opportunistic pruning on refresh: delete cache files older than about a day, and logs older than a week. A dozen lines in `store_last_refresh` / `log_path`.

## CLI has no `--help`/`--version`, and typos fall through to the JSON engine
- **Area**: UX
- **File**: src/lib.rs:24-40
- **Impact**: low
- **Description**: `herdr-worktrees --version` (or any misspelled subcommand) falls through to `model::run_engine`, which silently ignores unknown flags and dumps the full engine JSON — or errors with "not inside a git repository". For a binary the README tells people to invoke directly during development (`herdr plugin action invoke`, `dev-relink.sh`), a wrong invocation should say so.
- **Suggestion**: Handle `--version`/`-V` (print `env!("CARGO_PKG_VERSION")`) and `--help`, and make an unknown first argument that isn't an engine flag bail with a one-line usage listing the subcommands.
