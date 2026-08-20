# Code Quality Audit — herdr-worktrees

Audit date: 2026-08-19. All 17 source files (~5,900 lines) were read; `cargo clippy --all-targets -- -W clippy::pedantic` ran clean of errors (146 pedantic warnings); dead-code suspects verified with grep. Findings ordered by value. Cargo.toml is clean: all six dependencies (anyhow, indicatif, libc, serde, serde_json, toml) are used.

## Subprocess-with-timeout-and-SIGKILL block copy-pasted three times
- **Category**: duplication
- **File**: src/pr.rs:52-83, src/pr.rs:173-206, src/pr.rs:214-250
- **Effort**: small
- **Description**: `resolve_pr`, `gh_fetch_state`, and `gh_fetch_thread_counts` each contain a near-identical ~30-line block: spawn `gh`, grab the pid, spawn a waiter thread with an mpsc channel, `recv_timeout`, and on timeout `unsafe { libc::kill(pid, SIGKILL) }` — including the same three-line SAFETY comment. Only argv and timeout differ. This is exactly the tricky unsafe-adjacent code that should exist once.
- **Suggestion**: Extract `fn run_with_timeout(cmd: Command, timeout: Duration) -> Option<Output>` (or `gh_output(repo, args, timeout)`). Three call sites shrink to one line each; the SIGKILL/reaping logic gets a single home and a single test.

## `resolve_base_ref` implemented twice with the same fallback ladder
- **Category**: duplication
- **File**: src/git.rs:129-166, src/model.rs:428-473
- **Effort**: medium
- **Description**: Both implement the identical policy: configured `base-branch` (prefer `origin/<cfg>`, then local) → `origin/HEAD` symref → `main`/`master` on origin → local `main`/`master` → current branch. One queries git per candidate (`ref_exists`), the other reads a `RefSnapshot`. Any policy change (e.g. adding `trunk`) must land in two files or the picker's base and the engine's base silently diverge.
- **Suggestion**: Make the policy one function over an abstract lookup, e.g. `fn resolve_base(config, has_remote: impl Fn(&str) -> bool, has_local: impl Fn(&str) -> bool, origin_head: Option<&str>, current: &str) -> String`; git.rs and model.rs supply closures. One implementation, one test suite.

## Chunked scoped-thread fan-out duplicated three times
- **Category**: duplication
- **File**: src/remove.rs:238-272 (`inspect_candidates`), src/remove.rs:274-319 (`inspect_targets`), src/model.rs:248-284 (`compute`)
- **Effort**: medium
- **Description**: The pattern "`len.min(8)` workers, `div_ceil` chunk size, `thread::scope`, spawn per chunk, join, flatten, per-item error on panic" appears three times. The two in remove.rs are byte-for-byte identical apart from the mapped function, down to the repeated `(0..len).map(|_| Err(anyhow!("safety check panicked")))` recovery. model.rs has a third variant with a different worker count.
- **Suggestion**: Extract `fn parallel_map<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R>` in util.rs; the remove.rs panic-recovery variant wraps it. Removes ~70 lines and keeps the concurrency shape in one place.

## Duplicate render functions plus confirmed dead code
- **Category**: dead-code
- **File**: src/render.rs:84-118, src/model.rs:716-738, src/pr.rs:46
- **Effort**: small
- **Description**: Verified with grep:
  - `render_picker_row_with_options` (render.rs:102) has a body identical to `render_row_with_options` (render.rs:88); `render_picker_header_with_options` just forwards to `render_header_with_options`. The picker/non-picker split is vestigial.
  - The non-`_with_options` wrappers `render_row`, `render_picker_row`, `render_picker_header`, `render_header` are used only by tests (tests/engine.rs:76,424), never by production code.
  - `model::worktree_dirty` and `model::worktree_dirty_checked` (model.rs:716-738) have no callers anywhere; the remove flow uses its own `inspect_changes`.
  - `PullRequestTarget.url` (pr.rs:46) is populated and asserted in a test but never read by production code.
- **Suggestion**: Collapse the picker/plain render pairs into one function each, drop the zero-arg wrappers (tests pass `true`), delete `worktree_dirty*` and the `url` field. Since `pub` items in a lib produce no dead-code warnings, trim the public surface to what the binary dispatch needs or add a periodic unused-items pass.

## Sync status passed around as a swapped `(String, String)` tuple
- **Category**: idioms
- **File**: src/status.rs:8-64, src/model.rs:519-524, src/model.rs:587-618
- **Effort**: medium
- **Description**: `status::compute_sync` and friends return `(kind, display)` tuples, but `model::compute_one` destructures and swaps: `let (kind, text) = compute_sync(...); (text, kind)` (model.rs:522-523) into `(sync, sync_kind)`. Nothing stops a caller from getting the order wrong, and `model::compute_sync` returns tuple literals like `("loading".to_string(), "…".to_string())` where you must know which slot is which. The kind values ("synced", "ahead", "behind", "diverged", "local", "gone", "merged", "detached", "remote") are stringly-typed and string-matched in remove.rs:692 (`sync_has_unpublished`) and render.rs:60 (`sync_colored`).
- **Suggestion**: `struct SyncStatus { kind: SyncKind, display: String }` with `enum SyncKind` and `as_str()` for the fzf row protocol. `sync_has_unpublished` becomes a method, `sync_colored` a match on the enum, and the swap bug class disappears.

## The 6-field tab row protocol is implicit and hand-parsed everywhere
- **Category**: structure
- **File**: src/picker.rs:64-99, 130-134, 456-461, 548-578; src/model.rs:797-816
- **Effort**: medium
- **Description**: The row format (branch, path, entry-kind, sync-kind, changes, display) is encoded by `push_fzf_row` in model.rs and decoded independently at least five times in picker.rs via `sel.split('\t')` with magic indices, plus `six_field_row_key` counting tabs, plus manual field pushes in `append_create_row`/`append_pr_row` (`"\t\tcreate\t\t\t"`). The fzf flags `--with-nth=6`/`--accept-nth=1,2,3,4,5` depend on the same layout from another direction. `entry_route` maps kind strings to an enum, yet the ctrl-d handler still compares the raw string (`entry_kind == "worktree"`, picker.rs:89).
- **Suggestion**: Define `PickerRow { branch, path, kind: EntryRoute, sync_kind, changes, display }` with `to_line()`/`parse()` next to the field-count and `--with-nth` constants; use it in model.rs, all picker.rs handlers, and the create/PR row builders. One type documents and enforces the protocol.

## `RemovalRisk`: five enum variants encoding three booleans
- **Category**: idioms
- **File**: src/remove.rs:73-138, 500-514
- **Effort**: medium
- **Description**: `RemovalRisk` enumerates combinations (`Dirty`, `Unpublished`, `DirtyAndUnpublished`, `Detached`, `DirtyAndDetached`), then needs `dirty()`/`unpublished()`/`detached()` matches to project the flags back out, `description()`/`label()`/`code()`/`from_code()` with five arms each, and `removal_risk()` to build a variant from three bools — ~110 lines of combinatorial boilerplate. A fourth risk dimension would double the variant count.
- **Suggestion**: `struct RemovalRisk { dirty: bool, unpublished: bool, detached: bool }` (with `Option<RemovalRisk>` still meaning "safe"). `description`/`label` become joins over set flags, `code`/`from_code` a stable flag-list encoding, and `risk_is_covered` (remove.rs:739) reduces to three field comparisons.

## Two divergent parsers for `git status --porcelain`
- **Category**: duplication
- **File**: src/model.rs:686-711 (`status_counts`), src/remove.rs:672-690 (`parse_changes`)
- **Effort**: small
- **Description**: Both parse porcelain lines byte-by-byte with different rules: `status_counts` counts `??` into `unstaged` and matches explicit letter sets (`M|A|D|R|C` / `M|D`); `parse_changes` tracks `untracked` separately and treats any non-space first byte as staged. The difference is intentional (fast picker count vs. precise removal safety) but undocumented, and a fix to one (e.g. `U` conflict states) won't reach the other.
- **Suggestion**: Unify on one parser returning `{ staged, unstaged, untracked }` (remove.rs's `RemovalChanges` already has this shape) in a shared location; the picker derives its view from it. If the semantics must differ, colocate both with a comment stating the divergence.

## `pr.rs` atomic-JSON-write helper duplicated
- **Category**: duplication
- **File**: src/pr.rs:376-404 (`store_last_refresh`), src/pr.rs:419-449 (`cache_store`)
- **Effort**: small
- **Description**: Both repeat the same ~25 lines: bail on missing parent, `create_dir_all`, serialize, nonce'd `tmp-{pid}-{n}` temp name, `create_new` write, rename-or-remove. Only the payload struct and path differ. picker.rs:580-600 `atomic_replace_cache` is a third, slightly different sibling.
- **Suggestion**: Extract `fn write_json_atomic(path: &Path, value: &impl Serialize)`; optionally have picker's cache replacement reuse a `write_atomic(path, bytes)` core.

## `current_exe()` fallback expression repeated five times
- **Category**: duplication
- **File**: src/picker.rs:275-277, 996-998, 1012-1014; src/remove.rs:159-161, 413-415
- **Effort**: small
- **Description**: `std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "herdr-worktrees".to_string())` appears verbatim in five places (also flagged by clippy as `map().unwrap_or_else()` → `map_or_else`).
- **Suggestion**: Add `pub fn self_exe() -> String` to util.rs and call it everywhere.

## `picker::run` is a 136-line dispatch with inline handler bodies
- **Category**: structure
- **File**: src/picker.rs:20-173
- **Effort**: medium
- **Description**: Clippy flags it (too many lines, 136/100). The loop mixes arg parsing, the ctrl-p/ctrl-d/ctrl-n/alt-enter handlers (each with its own `split('\t')` parsing), and the selection routing; the ctrl-d branch alone is 20 lines of field extraction. The guarded `dry_name.as_deref().unwrap()` at line 39 also draws clippy's missing-`# Panics` warning.
- **Suggestion**: Once rows parse into a `PickerRow` (see protocol item), pull each key handler into a function returning a small `enum LoopAction { Exit, Refresh }`. Replace lines 38-40 with `if let (true, Some(name)) = (dry_run, &dry_name)`.

## Inconsistent error-reporting styles between picker actions and the rest
- **Category**: error-handling
- **File**: src/picker.rs:739-1030, src/remove.rs, src/git.rs:15-19
- **Effort**: medium
- **Description**: Three styles coexist: (1) picker action functions (`switch_worktree`, `create_worktree`, `checkout_remote_worktree`, `checkout_pr_worktree`) return `()` and report failures inline via `tty::err` + `tty::wait_key`, so callers can't distinguish success from failure and `picker::run` exits 0 on a failed create; (2) remove.rs threads `anyhow::Result` with `context()` throughout; (3) `git::git_stdout` maps any failure to `""`, which downstream turns into `parse().unwrap_or(0)` — errors silently become zeros/empties.
- **Suggestion**: Have picker action functions return `Result<()>` and centralize the `tty::err` + `wait_key` presentation in `picker::run` (one place decides "show and stay open"). Keep `git_stdout` but document its empty-on-failure contract, and prefer the checked `git_output` in paths that feed decisions, as remove.rs already does.

## `herdr::json` forces `Vec<&str>` re-collection boilerplate
- **Category**: naming
- **File**: src/herdr.rs:44-121
- **Effort**: small
- **Description**: `json(args: &[&str])` makes every dynamically-built call do `let refs: Vec<&str> = args.iter().map(String::as_str).collect();` — four times across `split_pane`, `open_worktree_pane`, `open_tab_pane`. Similarly `run(&["workspace".into(), ...])` forces `String` allocation for literals.
- **Suggestion**: Make the helpers generic like `Command::args`: `fn json<I, S>(args: I) -> Option<Value> where I: IntoIterator<Item = S>, S: AsRef<OsStr>`. The `.into()` and re-collect noise disappears.

## Misnamed `append_pr_row` prepends
- **Category**: naming
- **File**: src/picker.rs:568
- **Effort**: small
- **Description**: `append_pr_row` prepends the PR action row (its doc comment explains why it must come first), while its sibling `append_create_row` genuinely appends. Call-site readers will assume symmetric behavior.
- **Suggestion**: Rename to `prepend_pr_row`.

## Vestigial `<kind> <changes>` parameters threaded through the ctrl-d delete path
- **Category**: dead-code
- **File**: src/remove.rs:340-377, src/picker.rs:86-98
- **Effort**: small
- **Description**: `delete_worktree(branch, path, _kind, _changes, ...)` ignores its third and fourth parameters, yet picker.rs carefully extracts `del_kind`/`del_changes` to pass them, and the `remove --target` CLI (remove.rs:341) requires them in its usage string. The safety re-check made them obsolete but the plumbing remains.
- **Suggestion**: Drop both parameters from `delete_worktree` and simplify the picker's ctrl-d extraction. For the CLI, either drop the positionals or accept-and-ignore them for one release with a comment, since the subcommand is externally invocable.

## Clippy pedantic findings worth adopting (selective)
- **Category**: clippy
- **File**: multiple
- **Effort**: small
- **Description**: 146 pedantic warnings; most (68× `must_use`, 20× missing `# Errors` docs) are noise for an internal binary. Worth fixing:
  - src/render.rs:244-250 — `2629800`, `31557600`, `604800` lack `_` separators; `2_629_800` plus a "~1 month in seconds" comment makes `relative_age` auditable.
  - src/herdr.rs:84, src/remove.rs:211, src/remove.rs:840 — single-pattern `match` → `if let`/`let-else`.
  - src/theme.rs:142-143 — `"solarized" | "solarized-dark"` and `"solarized-light"` arms are identical; merge or comment that they deliberately share accents (it reads like a copy-paste slip).
  - src/picker.rs:385 — `unique_cache_path(directory: PathBuf, ...)` only reads the path; take `&Path` and drop the `to_path_buf()` at picker.rs:582.
  - 5× `map().unwrap_or_else()` → `map_or_else` (subsumed by the `self_exe()` extraction).
- **Suggestion**: Fix the listed items, then add `#![allow(clippy::must_use_candidate, clippy::missing_errors_doc)]` at the crate root and enable pedantic in CI so future useful lints aren't drowned out.

## Deliberately not flagged
- `strip_terminal_sequences`/`restore_ranked_rows` (picker.rs) are complex but well-tested and justified by the ANSI/OSC-preserving requirement.
- `model::Worktree` doing triple duty (worktree / local branch / remote candidate with sentinel `path: ""`, `sync: "remote"`) is a real design smell, but restructuring it into an enum ripples through serialization, rendering, and the fzf protocol — only worth doing after the row-protocol refactor, so it's folded into that item.
- `config::open_mode()` returning `"tab"`/`"workspace"` strings is stringly but tiny and locally contained.
