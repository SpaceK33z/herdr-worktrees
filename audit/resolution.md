# Audit Resolution — 2026-08-20

Status of every finding from bugs.md / improvements.md / code-quality.md after the fix effort (3 waves of agents). Final state: `cargo test` 126 passed (up from 47 at audit time), `cargo clippy --all-targets` clean, `cargo fmt --check` clean.

## Bugs (bugs.md) — 7/7 fixed

| Finding | Status |
|---|---|
| `git worktree add` failures swallowed ("see above") | Fixed — shared `worktree_add` helper with inherited stdio + `wait_key`, `-C repo` on all call sites |
| `status_counts` misses `U`/`T` codes | Fixed — unified porcelain parser handles unmerged/typechange; also fixed a latent `DD` double-count; regression tests for all six unmerged combos |
| `parse_color` panic on multibyte hex | Fixed — ASCII-hex validation before slicing; regression test reproduces the old panic |
| Same-repo PR checkout reuses stale local branch | Fixed — OID comparison vs `head_oid`; confirmed fast-forward via compare-and-swap `update-ref` when ancestor, refusal on divergence |
| Removal log path one-second collision | Fixed — `<kind>-<pid>-<nanos>-<attempt>.log` with existence probing |
| Kill-by-PID race after subprocess timeout | Fixed — `Arc<Mutex<Child>>` + `Child::kill()`; `libc::kill` removed from pr.rs; pipes drained on threads to avoid the 64KB deadlock |
| Progress pane loops forever if worker dies | Fixed — PID header in log, ESRCH liveness detection (2 consecutive polls), 120s no-header timeout |

## Improvements (improvements.md) — 12/12 addressed

| Finding | Status |
|---|---|
| PR cache written but never read | Fixed — `load` event uses `--cached`, ctrl-r bypasses; initial frame hydrates from cache read-only (`pr::cached_many`); README corrected |
| "see above" with nothing above | Fixed (see bugs) |
| Confirmations auto-accept when stdin not a TTY | Fixed — deny-by-default with explicit `IsTerminal` check; read errors also deny |
| PR failure message blames gh auth for everything | Fixed — `resolve_pr_detailed` distinguishes not-installed / timeout / gh stderr first line |
| Background GitHub failures invisible; 1.5s starves slow networks | Fixed — failure recorded in refresh state, footer shows "GitHub: failed — check gh auth"; interactive refresh gets 5s vs 1.5s background |
| Serial per-branch `git rev-list` | Fixed — branches computation parallelized with the shared chunked-scope pattern |
| Progress pane spin + log collision | Fixed (see bugs) |
| No branch-name validation | Fixed — `git check-ref-format --branch` before all create paths, with regression tests |
| ctrl-f fetch keybinding | Added — `picker-cache-fetch` runs `git fetch --quiet origin` + refresh; footer and README document it |
| Test gaps (base-ref, PR cache TTL, bg-batch protocol) | Fixed — precedence-ladder tests on the consolidated policy; TTL expiry + branch-validation cache tests; encode/decode round-trip tests |
| State directory grows without bound | Fixed — PR cache files pruned after ~1 day, logs after ~1 week, both opportunistic |
| No `--help`/`--version`; typos fall through to engine | Fixed — `-V/--version`, `-h/--help`, unknown-arg bail with usage |

## Code quality (code-quality.md) — 15/15 addressed

| Finding | Status |
|---|---|
| SIGKILL-timeout block ×3 | Fixed — single `run_with_timeout` + `gh_output` |
| `resolve_base_ref` ×2 | Fixed — one `git::base_ref_policy` over closures; both callers delegate |
| Chunked fan-out ×3 | Fixed — one `util::parallel_map`; worker-count and panic policy stay per caller |
| Duplicate render fns + dead code | Fixed — one row/header renderer each; `worktree_dirty*`, wrappers, `PullRequestTarget.url` deleted |
| Sync status as swapped tuple | Fixed — `SyncKind` enum + `SyncStatus` struct; wire/JSON byte-identical (serde round-trip asserted) |
| Implicit 6-field tab row protocol | Fixed — `src/row.rs` `PickerRow` with `to_line`/`parse`/`parse_selection`; fzf flags derived from `FIELD_COUNT` |
| `RemovalRisk` combinatorial enum | Fixed — three-bool struct + flag table; wire codes unchanged |
| Two porcelain parsers | Fixed — one `status::parse_porcelain`; callers document their divergent views; safety semantics preserved |
| Atomic-JSON-write ×3 | Fixed — `util::write_atomic` / `write_json_atomic` |
| `current_exe()` fallback ×5 | Fixed — `util::self_exe()` |
| 136-line `picker::run` | Fixed — five named handlers over a `PickerContext` |
| Inconsistent error reporting | Fixed — picker actions return `Result`; one `report()` presents err + wait_key. Behavior note: ctrl-d "could not start removal" errors are no longer silently swallowed |
| `herdr::json` re-collection boilerplate | Fixed — generic over `IntoIterator<Item = AsRef<OsStr>>` |
| `append_pr_row` misnomer | Fixed — renamed `prepend_pr_row` |
| Vestigial ctrl-d kind/changes params | Fixed — dropped from `delete_worktree` and the `remove --target` argv (no external callers, verified) |
| Clippy pedantic selections | Fixed — digit separators, single-pattern matches, solarized arms merged, `&Path` param, `map_or_else` |

## Notes

- A concurrent session (not part of this effort) developed a worktree-path auto-detection feature (`src/detect.rs`, config/setup/include changes) and committed f0d2013, which also swept in most of this effort's then-uncommitted changes. The wave-3 structural refactors in git.rs, picker.rs, pr.rs, remove.rs, row.rs, util.rs remain uncommitted on top of it.
- Deliberately not done (per audit's own recommendation): restructuring `model::Worktree`'s triple duty into an enum (folded into a possible future row-protocol iteration); `config::open_mode()` stringly-typed but tiny.
