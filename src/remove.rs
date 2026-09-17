//! The worktree delete popup (`prefix+d`), and one-off deletes from the picker.

use crate::background;
use crate::config::Config;
use crate::git;
use crate::herdr;
use crate::model;
use crate::render;
use crate::status::{self, ChangeCounts, SyncKind};
use crate::tty;
use crate::util;
use anyhow::{Context as _, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

const COL_SAFETY: usize = 16;
const PROGRESS_DONE: &str = "__HERDR_WORKTREE_REMOVE_DONE__";
const PROGRESS_UPDATE: &str = "__HERDR_WORKTREE_REMOVE_UPDATE__";
const PROGRESS_PID: &str = "__HERDR_WORKTREE_REMOVE_PID__";
/// Fields per target in the `remove-bg-batch` argv protocol.
const BATCH_FIELDS: usize = 4;
/// Wire code for "no risks", the one verdict `RemovalRisk` cannot encode.
const SAFE_CODE: &str = "safe";
/// How long the follower waits for the worker's pid header before deciding the
/// worker never started. The header is written before any git work, so this
/// only has to cover process startup.
const PROGRESS_PID_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
struct RemovalTarget {
    branch: String,
    path: String,
}

#[derive(Debug, Clone)]
struct PreparedRemoval {
    branch: String,
    path: String,
    head: String,
    authorized_risk: Option<RemovalRisk>,
    /// How the branch delete would be made safe. `None` when it was not
    /// evaluated (branch preservation off, or a detached HEAD). The detached
    /// worker recomputes this rather than trusting argv.
    safety: Option<DeletionSafety>,
    /// Another registered worktree still holds this branch checked out.
    /// Deleting the ref would leave that worktree unable to resolve HEAD, so
    /// the branch must be kept whatever else is true.
    kept_by_checkout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetentionProof {
    reference: String,
    oid: String,
}

/// Evidence that deleting a branch loses no work. Both safe variants retain
/// their supporting ref/OID for the final branch-deletion transaction; only an
/// explicitly authorized unpublished delete uses branch-HEAD-only CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeletionSafety {
    /// Every commit is on the upstream named by the proof; the delete
    /// transaction verifies its pinned OID before removing the branch.
    Published(RetentionProof),
    /// The branch's content already lives in the base branch — same commit,
    /// ancestor, empty diff, matching trees, or a merge that adds nothing —
    /// even though ancestry and upstream tracking cannot show it (squash
    /// merges, rebases). The string says which probe matched.
    Integrated(String, RetentionProof),
    /// Commits may exist nowhere else; deletion needs explicit confirmation.
    Unpublished,
}

struct RemovalInspection {
    prepared: PreparedRemoval,
    changes: ChangeCounts,
    /// Dimmed tag shown after the safety cell explaining why the branch is
    /// deletable (`pushed`, `merged`). Display-only: the detached worker
    /// recomputes the real verdict, so this never crosses the remove-bg-batch
    /// argv protocol.
    safety_note: Option<String>,
}

/// The removal picker's changes column. Unlike the switch picker it has room to
/// name untracked files separately, because they are the work most easily lost.
fn changes_display(changes: ChangeCounts) -> String {
    let ChangeCounts {
        staged,
        unstaged,
        untracked,
    } = changes;
    if !changes.dirty() {
        "clean".to_string()
    } else if staged == 0 && unstaged == 0 {
        "untracked".to_string()
    } else if untracked == 0 {
        format!("+{staged} ~{unstaged}")
    } else {
        format!("+{staged} ~{unstaged} ?{untracked}")
    }
}

/// One reason a removal is not obviously safe. The table order defines the
/// order of flags in `RemovalRisk::code`, and must stay in step with
/// `RemovalRisk::flags`.
struct RiskFlag {
    /// Wire token used by the `remove-bg-batch` argv protocol.
    code: &'static str,
    /// Phrase for the confirmation prompt.
    description: &'static str,
    /// Column label when this is the only risk.
    label: &'static str,
    /// Column label when several risks share the 16-column safety cell.
    short_label: &'static str,
}

static RISK_FLAGS: [RiskFlag; 3] = [
    RiskFlag {
        code: "dirty",
        description: "uncommitted changes",
        label: "dirty",
        short_label: "dirty",
    },
    RiskFlag {
        code: "unpublished",
        description: "unpublished commits",
        label: "unpublished",
        short_label: "unpub",
    },
    RiskFlag {
        code: "detached",
        description: "a detached HEAD",
        label: "detached",
        short_label: "detach",
    },
];

/// The risks a removal carries. `Option<RemovalRisk>` is the safety verdict:
/// `None` means safe, `Some` means at least one flag is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemovalRisk {
    dirty: bool,
    unpublished: bool,
    detached: bool,
}

impl RemovalRisk {
    fn flags(self) -> [bool; 3] {
        [self.dirty, self.unpublished, self.detached]
    }

    fn set_flags(self) -> Vec<&'static RiskFlag> {
        RISK_FLAGS
            .iter()
            .zip(self.flags())
            .filter_map(|(flag, set)| set.then_some(flag))
            .collect()
    }

    fn description(self) -> String {
        join_phrases(
            &self
                .set_flags()
                .iter()
                .map(|flag| flag.description)
                .collect::<Vec<_>>(),
        )
    }

    fn label(self) -> String {
        let flags = self.set_flags();
        let labels: Vec<_> = if flags.len() == 1 {
            flags.iter().map(|flag| flag.label).collect()
        } else {
            flags.iter().map(|flag| flag.short_label).collect()
        };
        format!("⚠ {}", labels.join(" + "))
    }

    /// Wire encoding: the set flags' codes joined by `-`, e.g.
    /// `dirty-unpublished`.
    fn code(self) -> String {
        self.set_flags()
            .iter()
            .map(|flag| flag.code)
            .collect::<Vec<_>>()
            .join("-")
    }

    fn from_code(code: &str) -> Option<Self> {
        let mut risk = Self {
            dirty: false,
            unpublished: false,
            detached: false,
        };
        for token in code.split('-') {
            let index = RISK_FLAGS.iter().position(|flag| flag.code == token)?;
            let flags = [&mut risk.dirty, &mut risk.unpublished, &mut risk.detached];
            if *flags[index] {
                // A repeated token is a malformed code, not a stronger risk.
                return None;
            }
            *flags[index] = true;
        }
        Some(risk)
    }
}

/// "a", "a and b", "a, b and c".
fn join_phrases(phrases: &[&str]) -> String {
    match phrases {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

pub fn run_interactive() -> Result<()> {
    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    let list = render_remove_candidates(&repo, &config, &state_dir, false);

    let header = format!(
        "{}  {}",
        render::pad("safety", COL_SAFETY),
        render::render_header_with_options(config.show_worktree_name())
    );
    let footer = "tab select · shift-tab deselect · enter remove selected/current · ctrl-r recheck";
    if list.is_empty() {
        println!("\x1b[33mNo removable worktrees (only the main checkout exists).\x1b[0m");
        tty::wait_key();
        return Ok(());
    }

    let exe = crate::util::self_exe();
    let refresh_cmd = format!("{} remove-list", crate::util::shell_escape(&exe));
    let cur_path = git::current_toplevel();
    let selections = run_remove_fzf(&list, &header, footer, &cur_path, &refresh_cmd)?;
    let targets = parse_targets(&selections);
    if targets.is_empty() {
        return Ok(());
    }
    delete_worktrees(&targets, &config, &repo, RemoveOptions::default())
}

/// Print the untracked-aware candidate list used by fzf's background reload.
pub fn run_list(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        anyhow::bail!("usage: remove-list");
    }
    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    print!(
        "{}",
        render_remove_candidates(&repo, &config, &state_dir, true)
    );
    Ok(())
}

fn render_remove_candidates(
    repo: &str,
    config: &Config,
    state_dir: &Path,
    inspect: bool,
) -> String {
    let engine = if inspect {
        model::compute_remove_snapshot(repo, config, state_dir)
    } else {
        model::compute_remove_initial(repo, config, state_dir)
    };
    let removable: Vec<_> = engine
        .worktrees
        .iter()
        .filter(|worktree| !util::same_path(&worktree.path, repo))
        .collect();
    let inspections = inspect.then(|| inspect_candidates(&removable, config, repo));
    let mut rows = Vec::with_capacity(removable.len());

    for (index, worktree) in removable.into_iter().enumerate() {
        let branch = branch_name(worktree);
        let mut display = worktree.clone();
        let safety = if let Some(inspections) = &inspections {
            if let Ok(inspection) = &inspections[index] {
                display.staged = inspection.changes.staged;
                display.unstaged = inspection.changes.unstaged;
                display.dirty = inspection.changes.dirty();
                display.changes = changes_display(inspection.changes);
                append_safety_tag(
                    &render_safety(inspection.prepared.authorized_risk),
                    inspection.safety_note.as_deref(),
                )
            } else {
                display.changes = "unknown".to_string();
                render_unverified()
            }
        } else {
            render_checking()
        };
        let row = render::render_row_with_options(
            &branch,
            &display,
            &engine.prefix,
            engine.show_worktree_name,
        );
        rows.push(format!("{branch}\t{}\t{safety}  {row}", worktree.path));
    }
    rows.join("\n")
}

/// How many safety checks run at once. Deliberately modest: each one is a
/// `git status` over a whole worktree, and the list is short.
const INSPECT_WORKERS: usize = 8;

/// Run `inspect` over `items` in parallel. A panicking check turns into a
/// per-item error instead of a missing row — an unverified row must never look
/// safe.
fn inspect_in_parallel<T: Sync, R: Send>(
    items: &[T],
    inspect: impl Fn(&T) -> Result<R> + Sync,
) -> Vec<Result<R>> {
    util::parallel_map(items, INSPECT_WORKERS, inspect)
        .into_iter()
        .map(|inspected| inspected.unwrap_or_else(|| Err(anyhow::anyhow!("safety check panicked"))))
        .collect()
}

fn inspect_candidates(
    worktrees: &[&model::Worktree],
    config: &Config,
    repo: &str,
) -> Vec<Result<RemovalInspection>> {
    inspect_in_parallel(worktrees, |worktree| {
        inspect_candidate(worktree, config, repo)
    })
}

fn inspect_targets(
    targets: &[RemovalTarget],
    config: &Config,
    repo: &str,
) -> Vec<Result<PreparedRemoval>> {
    if targets.is_empty() {
        return Vec::new();
    }
    let records = match registered_worktrees(repo) {
        Ok(records) => records,
        Err(error) => {
            let message = format!("{error:#}");
            return targets
                .iter()
                .map(|_| Err(anyhow::anyhow!(message.clone())))
                .collect();
        }
    };
    inspect_in_parallel(targets, |target| {
        inspect_target_in_records(target, &records, config, repo)
    })
}

fn inspect_candidate(
    worktree: &model::Worktree,
    config: &Config,
    repo: &str,
) -> Result<RemovalInspection> {
    let changes = inspect_changes(&worktree.path)?;
    let detached = worktree.branch.is_empty();
    let deletable = config.delete_branch() && !detached;
    let mut unpublished = deletable && sync_has_unpublished(worktree.sync_kind);
    // Why the branch is deletable, for the dimmed safety tag. A sync state
    // that already proves every commit reached the upstream needs no git
    // calls; only ambiguous states justify running the probe ladder.
    let safety_note = if !deletable {
        None
    } else if !unpublished {
        Some("pushed".to_string())
    } else {
        let safety = branch_deletion_safety(config, repo, &worktree.branch)?;
        // The probe outranks the sync heuristic: content proven to live on
        // the base branch is deletable even when the sync column still says
        // ahead/local/gone. Display-only either way — the removal re-inspects
        // authoritatively before deleting anything.
        unpublished = matches!(safety, DeletionSafety::Unpublished);
        deletion_safety_tag(&safety).map(str::to_string)
    };
    Ok(RemovalInspection {
        prepared: PreparedRemoval {
            branch: branch_name(worktree),
            path: worktree.path.clone(),
            head: worktree.head.clone(),
            authorized_risk: removal_risk(changes.dirty(), unpublished, detached),
            safety: None,
            kept_by_checkout: None,
        },
        changes,
        safety_note,
    })
}

/// Compact label for why a branch may be auto-deleted. `Unpublished` gets no
/// tag: the ⚠ label already carries that warning.
fn deletion_safety_tag(safety: &DeletionSafety) -> Option<&'static str> {
    match safety {
        DeletionSafety::Published(_) => Some("pushed"),
        DeletionSafety::Integrated(..) => Some("merged"),
        DeletionSafety::Unpublished => None,
    }
}

/// Dimmed explanation appended after the padded safety cell. It lives inside
/// the final fzf column, after the second tab, so the identity columns used
/// by `parse_targets` stay untouched.
fn append_safety_tag(safety: &str, note: Option<&str>) -> String {
    match note {
        Some(tag) => format!("{safety}\x1b[2m ·{tag}\x1b[0m"),
        None => safety.to_string(),
    }
}

const TARGET_USAGE: &str = "usage: remove --target <branch> <path> [--yes|-y] [--force|-f]";

/// Command-line overrides for the confirmation step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoveOptions {
    /// Skip the confirmation prompt, so the removal works without a TTY.
    /// Risky targets are still refused unless `force` is set.
    pub assume_yes: bool,
    /// Remove despite dirty files, unpublished commits, or a detached HEAD;
    /// the same as `[remove].force = true`.
    pub force: bool,
}

fn parse_target_args(args: &[String]) -> Result<(&str, &str, RemoveOptions)> {
    let [target, branch, path, flags @ ..] = args else {
        anyhow::bail!("{TARGET_USAGE}");
    };
    if target != "--target" || branch.is_empty() || path.is_empty() {
        anyhow::bail!("{TARGET_USAGE}");
    }
    let mut options = RemoveOptions::default();
    for flag in flags {
        match flag.as_str() {
            "--yes" | "-y" => options.assume_yes = true,
            "--force" | "-f" => options.force = true,
            other => anyhow::bail!("unknown argument '{other}'\n{TARGET_USAGE}"),
        }
    }
    Ok((branch, path, options))
}

/// The `remove --target <branch> <path>` one-off delete used by the picker's
/// ctrl-d, and by scripts with `--yes`. The row's cached kind and changes used
/// to be passed along too; the removal re-inspects both, so they are no longer
/// part of the protocol.
pub fn run_target(args: &[String]) -> Result<()> {
    let (branch, path, options) = parse_target_args(args)?;

    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    if util::same_path(path, &repo) {
        tty::err("the main checkout can't be removed");
        return Ok(());
    }
    let config = Config::load()?;
    delete_worktree(branch, path, &config, &repo, options)
}

/// Confirm, remove the checkout, optionally delete the branch, and close the
/// Herdr workspace that was open for it.
pub fn delete_worktree(
    branch: &str,
    path: &str,
    config: &Config,
    repo: &str,
    options: RemoveOptions,
) -> Result<()> {
    delete_worktrees(
        &[RemovalTarget {
            branch: branch.to_string(),
            path: path.to_string(),
        }],
        config,
        repo,
        options,
    )
}

fn delete_worktrees(
    targets: &[RemovalTarget],
    config: &Config,
    repo: &str,
    options: RemoveOptions,
) -> Result<()> {
    if targets
        .iter()
        .any(|target| util::same_path(&target.path, repo))
    {
        tty::err("the main checkout can't be removed");
        return Ok(());
    }

    // A path typed by hand can be relative to the shell's cwd, or reach the
    // worktree through a symlinked parent. Everything past this point — the
    // progress pane's cwd, the detached worker's argv, `git worktree remove`
    // itself — needs the absolute path git recorded, so resolve the spelling
    // once, here.
    let targets = &resolve_registered_paths(targets, repo);

    // Re-check identity, HEAD, dirty files, and publication state immediately
    // before confirmation. A failed probe cancels rather than becoming `safe`.
    let mut prepared = Vec::with_capacity(targets.len());
    for (target, inspection) in targets.iter().zip(inspect_targets(targets, config, repo)) {
        match inspection {
            Ok(target) => prepared.push(target),
            Err(error) => {
                tty::err(&format!(
                    "could not verify '{}'; nothing was removed: {error:#}",
                    target.branch
                ));
                tty::wait_key();
                return Ok(());
            }
        }
    }
    let risks: Vec<_> = prepared
        .iter()
        .filter_map(|target| target.authorized_risk)
        .collect();
    let warn = !(risks.is_empty() || options.force || config.force());
    if options.assume_yes {
        // `--yes` answers the plain prompt only: a script must say `--force`
        // to discard work, the same way an interactive user sees the warning.
        if warn {
            anyhow::bail!(
                "{}; nothing was removed — pass --force to remove anyway",
                removal_refusal(targets, &risks)
            );
        }
    } else if !tty::confirm(&removal_prompt(targets, &risks, warn)) {
        return Ok(());
    }

    // One detached process removes the whole selection and sends one summary
    // notification, rather than opening a process and notification per row.
    let exe = crate::util::self_exe();
    let log = background::log_path("remove");
    std::fs::File::create(&log).context("creating removal progress log")?;
    open_progress_pane(targets, config, repo, &log, &exe);

    let args = encode_batch_args(repo, &prepared);
    if let Err(error) = background::spawn_detached(&exe, &args, &log) {
        append_progress(&log, &format!("Could not start removal: {error}"));
        append_progress(&log, PROGRESS_DONE);
        return Err(error.into());
    }
    if targets.len() == 1 {
        println!("removing '{}' in the background…", targets[0].branch);
    } else {
        println!("removing {} worktrees in the background…", targets.len());
    }
    Ok(())
}

fn open_progress_pane(
    targets: &[RemovalTarget],
    config: &Config,
    repo: &str,
    log: &str,
    exe: &str,
) {
    let label = if targets.len() == 1 {
        format!("Removing {}", targets[0].branch)
    } else {
        format!("Removing {} worktrees", targets.len())
    };

    let pane = if targets.len() == 1 && config.open_mode() == "workspace" {
        let target = &targets[0];
        match herdr::worktree_workspace_id(&target.path, repo) {
            Some(workspace) => herdr::open_tab_pane(Some(&workspace), &target.path, &label, true),
            None => herdr::open_worktree_pane(
                herdr::root_workspace(repo).as_deref(),
                repo,
                &target.path,
                &label,
                true,
            ),
        }
    } else {
        let cwd = if targets.len() == 1 {
            targets[0].path.as_str()
        } else {
            repo
        };
        herdr::open_tab_pane(herdr::current_workspace().as_deref(), cwd, &label, true)
    };

    if let Some(pane) = pane {
        let command = format!(
            "{} remove-progress {}; exit",
            crate::util::shell_escape(exe),
            crate::util::shell_escape(log),
        );
        herdr::run_in_pane(&pane, &command);
    }
}

fn append_progress(log: &str, message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(file, "{message}");
        let _ = file.flush();
    }
}

fn removal_risk(dirty: bool, unpublished: bool, detached: bool) -> Option<RemovalRisk> {
    // A detached HEAD has no branch, so there is nothing to publish.
    let unpublished = unpublished && !detached;
    (dirty || unpublished || detached).then_some(RemovalRisk {
        dirty,
        unpublished,
        detached,
    })
}

fn render_safety(risk: Option<RemovalRisk>) -> String {
    let (label, color) = match risk {
        Some(risk) => (risk.label(), if risk.dirty { "31" } else { "33" }),
        None => ("✓ safe".to_string(), "32"),
    };
    format!("\x1b[{color}m{}\x1b[0m", render::pad(&label, COL_SAFETY))
}

fn render_unverified() -> String {
    format!(
        "\x1b[31m{}\x1b[0m",
        render::pad("✕ verify failed", COL_SAFETY)
    )
}

fn render_checking() -> String {
    format!("\x1b[33m{}\x1b[0m", render::pad("… checking", COL_SAFETY))
}

fn removal_prompt(targets: &[RemovalTarget], risks: &[RemovalRisk], warn: bool) -> String {
    if targets.len() == 1 {
        let branch = &targets[0].branch;
        if warn {
            return format!(
                "  ⚠ '{branch}' has {} — enter to remove anyway, any other key to cancel",
                risks[0].description()
            );
        }
        return format!("  remove '{branch}'? enter to confirm, any other key to cancel");
    }

    if warn {
        format!(
            "  ⚠ {} of {} selected worktrees are not safe to remove — enter to remove all anyway, any other key to cancel",
            risks.len(),
            targets.len()
        )
    } else {
        format!(
            "  remove {} selected worktrees? enter to confirm, any other key to cancel",
            targets.len()
        )
    }
}

/// The `--yes` counterpart of a warning prompt: what stops the removal.
fn removal_refusal(targets: &[RemovalTarget], risks: &[RemovalRisk]) -> String {
    if targets.len() == 1 {
        format!("'{}' has {}", targets[0].branch, risks[0].description())
    } else {
        format!(
            "{} of {} selected worktrees are not safe to remove",
            risks.len(),
            targets.len()
        )
    }
}

fn parse_targets(selections: &[String]) -> Vec<RemovalTarget> {
    selections
        .iter()
        .filter_map(|selection| {
            let mut parts = selection.split('\t');
            let branch = parts.next()?;
            let path = parts.next()?;
            if branch.is_empty() || path.is_empty() {
                return None;
            }
            Some(RemovalTarget {
                branch: branch.to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

fn branch_name(worktree: &model::Worktree) -> String {
    if worktree.branch.is_empty() {
        "(detached)".to_string()
    } else {
        worktree.branch.clone()
    }
}

fn inspect_target(target: &RemovalTarget, config: &Config, repo: &str) -> Result<PreparedRemoval> {
    let records = registered_worktrees(repo)?;
    inspect_target_in_records(target, &records, config, repo)
}

/// Rewrite each target's path to the one git has registered for that
/// directory, so a relative or symlinked spelling names the same worktree. A
/// path that matches nothing is left alone: inspection reports it as
/// unregistered, which is the honest answer. A failed listing is left to the
/// inspection step to report too.
fn resolve_registered_paths(targets: &[RemovalTarget], repo: &str) -> Vec<RemovalTarget> {
    let records = registered_worktrees(repo).unwrap_or_default();
    targets
        .iter()
        .map(|target| {
            let path = records
                .iter()
                .find(|record| util::same_path(&record.path, &target.path))
                .map_or_else(|| target.path.clone(), |record| record.path.clone());
            RemovalTarget {
                branch: target.branch.clone(),
                path,
            }
        })
        .collect()
}

fn registered_worktrees(repo: &str) -> Result<Vec<model::RawWorktree>> {
    let output = git::git_output(&["-C", repo, "worktree", "list", "--porcelain"])
        .context("listing registered worktrees")?;
    if !output.status.success() {
        anyhow::bail!("git worktree list failed");
    }
    Ok(model::parse_worktree_list(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn inspect_target_in_records(
    target: &RemovalTarget,
    records: &[model::RawWorktree],
    config: &Config,
    repo: &str,
) -> Result<PreparedRemoval> {
    let record = records
        .iter()
        .find(|record| util::same_path(&record.path, &target.path))
        .with_context(|| format!("{} is no longer a registered worktree", target.path))?;
    let actual_branch = if record.branch.is_empty() {
        "(detached)"
    } else {
        &record.branch
    };
    if actual_branch != target.branch {
        anyhow::bail!(
            "worktree branch changed from '{}' to '{actual_branch}'",
            target.branch
        );
    }

    let changes = inspect_changes(&record.path)?;
    let detached = record.branch.is_empty();
    let (safety, unpublished) = if config.delete_branch() && !detached {
        let safety = branch_deletion_safety_at(config, repo, &record.branch, &record.head)?;
        let unpublished = matches!(safety, DeletionSafety::Unpublished);
        (Some(safety), unpublished)
    } else {
        // Branch preservation mode: nothing will be deleted, so there is no
        // publication state to guard.
        (None, false)
    };
    // A second checkout of the branch (only reachable via `git worktree add
    // --force`, or a later switch inside another worktree) keeps the ref
    // alive: deleting it would break that worktree's HEAD.
    let kept_by_checkout = records
        .iter()
        .find(|other| {
            other.path != record.path && !other.branch.is_empty() && other.branch == record.branch
        })
        .map(|other| other.path.clone());

    Ok(PreparedRemoval {
        branch: target.branch.clone(),
        path: record.path.clone(),
        head: record.head.clone(),
        authorized_risk: removal_risk(changes.dirty(), unpublished, detached),
        safety,
        kept_by_checkout,
    })
}

fn inspect_changes(path: &str) -> Result<ChangeCounts> {
    let output = git::git_output(&[
        "--no-optional-locks",
        "-C",
        path,
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    ])
    .with_context(|| format!("checking worktree status for {path}"))?;
    if !output.status.success() {
        anyhow::bail!("git status failed for {path}");
    }

    Ok(status::parse_porcelain(&output.stdout))
}

/// Which sync states may still hold commits that exist nowhere else. Anything
/// the picker could not place — a missing upstream, a state it has not computed
/// yet — counts as unpublished, so deleting the branch needs confirmation.
fn sync_has_unpublished(sync_kind: SyncKind) -> bool {
    !matches!(sync_kind, SyncKind::Synced | SyncKind::Behind)
}

#[cfg(test)]
fn branch_publication(repo: &str, branch: &str) -> Result<Option<RetentionProof>> {
    let head = git::ref_oid(repo, &format!("refs/heads/{branch}")).context("branch disappeared")?;
    branch_publication_at(repo, branch, &head)
}

fn branch_publication_at(repo: &str, branch: &str, head: &str) -> Result<Option<RetentionProof>> {
    let Some(upstream) = git::branch_upstream(repo, branch) else {
        // Failure to resolve an upstream is conservative: keeping the branch is
        // safe, deleting it needs explicit force confirmation.
        return Ok(None);
    };
    let oid = git::git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(objectname)",
        &upstream,
    ]);
    let oid = oid.trim();
    if oid.is_empty() {
        return Ok(None);
    }

    // Compare against the captured object, not the mutable ref name. The same
    // OID is verified in the update-ref transaction before branch deletion.
    let range = format!("{oid}..{head}");
    let output = git::git_output(&["-C", repo, "rev-list", "--count", &range])
        .context("checking unpublished commits")?;
    if !output.status.success() {
        anyhow::bail!("git rev-list failed while checking {branch}");
    }
    let count: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("parsing unpublished commit count")?;
    if count > 0 {
        Ok(None)
    } else {
        Ok(Some(RetentionProof {
            reference: upstream,
            oid: oid.to_string(),
        }))
    }
}

/// Combine the upstream proof with content-integration probes: a branch whose
/// upstream is gone or stale can still be safe to delete when its content has
/// already landed on the base branch (squash merges, rebases).
fn branch_deletion_safety(config: &Config, repo: &str, branch: &str) -> Result<DeletionSafety> {
    let head = git::ref_oid(repo, &format!("refs/heads/{branch}")).context("branch disappeared")?;
    branch_deletion_safety_at(config, repo, branch, &head)
}

fn branch_deletion_safety_at(
    config: &Config,
    repo: &str,
    branch: &str,
    head: &str,
) -> Result<DeletionSafety> {
    if let Some(proof) = branch_publication_at(repo, branch, head)? {
        return Ok(DeletionSafety::Published(proof));
    }
    if let Some(target) = integration_target(config, repo, branch) {
        if let Some(oid) = git::ref_oid(repo, &target) {
            if let Some(reason) = integration_reason_at(repo, branch, head, &target, &oid) {
                return Ok(DeletionSafety::Integrated(
                    reason,
                    RetentionProof {
                        reference: target,
                        oid,
                    },
                ));
            }
        }
    }
    Ok(DeletionSafety::Unpublished)
}

/// How many commits the patch-id squash-merge fallback is willing to walk on
/// the target side.
///
/// [`is_squash_merged_via_patch_id`] runs `git rev-list <merge-base>..<target>
/// | git diff-tree --stdin -p | git patch-id` — one patch per commit the base
/// gained since the branch diverged. On a fast-moving repo an old branch can
/// sit thousands of commits behind the tip, turning one integration check
/// into seconds of work. A cheap `git rev-list --count` pre-flight enforces
/// this cap. 500 is conservative: per-commit cost scales with changed files ×
/// changed lines, so a few hundred lockfile-bump squashes are slower than a
/// few thousand tiny commits, yet a normal review-and-cleanup cycle sits well
/// inside the limit. A branch squash-merged further back than the cap is
/// reported as *not* integrated — the safe direction.
const PATCH_ID_SCAN_MAX_COMMITS: usize = 500;

/// Why a branch's content is already in the base branch, even though git
/// ancestry and upstream tracking cannot show it. Probes mirror worktrunk's
/// branch-cleanup ladder, cheapest first; any single hit means deleting the
/// ref loses no work. Every command is a plumbing query — nothing touches a
/// working tree, and every failure conservatively reports "not integrated".
#[cfg(test)]
fn branch_integration_reason(config: &Config, repo: &str, branch: &str) -> Option<String> {
    let local_ref = format!("refs/heads/{branch}");
    let branch_oid = git::ref_oid(repo, &local_ref)?;
    let target = integration_target(config, repo, branch)?;
    let target_oid = git::ref_oid(repo, &target)?;
    integration_reason_at(repo, branch, &branch_oid, &target, &target_oid)
}

fn integration_reason_at(
    repo: &str,
    branch: &str,
    branch_oid: &str,
    target: &str,
    target_oid: &str,
) -> Option<String> {
    // 1. Same commit — the branch points at the base tip.
    if branch_oid == target_oid {
        return Some(format!("{branch} is at the same commit as {target}"));
    }

    // 2. Ancestor — every branch commit is in the base history (fast-forward
    //    or rebase case).
    if git::git_success(&[
        "-C",
        repo,
        "merge-base",
        "--is-ancestor",
        branch_oid,
        target_oid,
    ]) {
        return Some(format!("{branch} is contained in {target}"));
    }

    // 3. No added changes — the diff from the merge-base is empty.
    let base = merge_base(repo, target_oid, branch_oid);
    if let Some(base) = &base {
        let range = format!("{base}..{branch_oid}");
        let files = git::git_output(&["-C", repo, "diff", "--name-only", &range]).ok()?;
        // Failed inspection is never positive integration evidence.
        if !files.status.success() {
            return None;
        }
        if files.stdout.is_empty() {
            return Some(format!("{branch} adds no file changes to {target}"));
        }
    }

    // 4. Trees match — identical content despite different history.
    let branch_tree = commit_tree(repo, branch_oid);
    let target_tree = commit_tree(repo, target_oid);
    if branch_tree.is_some() && branch_tree == target_tree {
        return Some(format!("{branch} content matches {target}"));
    }

    // 5. Merge adds nothing — simulating the merge reproduces the base tree,
    //    which covers squash merges where the base advanced with changes to
    //    other files. Needs git >= 2.38 (`merge-tree --write-tree`); older
    //    git skips the probe. Exit code 1 means the simulated merge
    //    conflicts — usually because the base later touched the same files
    //    the (already squashed) branch changed — and hands over to the
    //    patch-id fallback; every other failure skips it.
    let conflicted = match (
        target_tree.as_deref(),
        git::git_output(&[
            "-C",
            repo,
            "merge-tree",
            "--write-tree",
            target_oid,
            branch_oid,
        ]),
    ) {
        (Some(target_tree), Ok(output)) if output.status.success() => {
            let merged = String::from_utf8_lossy(&output.stdout);
            if merged.lines().next().map(str::trim) == Some(target_tree) {
                return Some(format!("merging {branch} into {target} adds nothing"));
            }
            false
        }
        (_, Ok(output)) if output.status.code() == Some(1) => true,
        _ => false,
    };

    // 6. Patch-id match — when merging conflicts, look for a commit on the
    //    base whose entire diff hashes identically to the branch's combined
    //    diff. That commit IS the squash merge, however much the two
    //    histories have since diverged around it.
    if conflicted {
        if let Some(base) = base.as_deref() {
            if let Some(reason) =
                is_squash_merged_via_patch_id(repo, target, branch, branch_oid, target_oid, base)
            {
                return Some(reason);
            }
        }
    }

    None
}

/// Detect a squash merge by patch-id matching.
///
/// Hashes the branch's entire diff against the merge-base and checks whether
/// any single commit on the target hashes to the same value — a match means
/// the target contains exactly the branch's changes as one commit, whatever
/// else landed around it.
///
/// Both sides generate their diffs with `git diff-tree` (plumbing), never
/// `git log -p`: porcelain honors the user's `diff.context` / `diff.algorithm`
/// config while plumbing ignores it, so a mismatched pair could hash the same
/// change differently and never agree.
fn is_squash_merged_via_patch_id(
    repo: &str,
    target: &str,
    branch: &str,
    branch_oid: &str,
    target_oid: &str,
    merge_base: &str,
) -> Option<String> {
    // Bound the target-side walk before any diffing starts. See
    // [`PATCH_ID_SCAN_MAX_COMMITS`] for why this must be cheap.
    let count: usize = git::git_stdout(&[
        "-C",
        repo,
        "rev-list",
        "--count",
        &format!("{merge_base}..{target_oid}"),
    ])
    .trim()
    .parse()
    .ok()?;
    if count > PATCH_ID_SCAN_MAX_COMMITS {
        return None;
    }

    // The branch side diffs the merge-base tree against the branch tip — one
    // patch for the whole branch.
    let branch_pids = patch_ids_from(repo, &["diff-tree", "-p", merge_base, branch_oid], None)?;
    let branch_pid = branch_pids.split_whitespace().next()?;

    // The target side gets its commit list from rev-list (small, so buffered)
    // and streams one diff per commit into patch-id in a single pass.
    let commits = git::git_stdout(&[
        "-C",
        repo,
        "rev-list",
        &format!("{merge_base}..{target_oid}"),
    ]);
    let target_pids = patch_ids_from(
        repo,
        &["diff-tree", "--stdin", "-p"],
        Some(commits.into_bytes()),
    )?;
    if target_pids
        .lines()
        .any(|line| line.split_whitespace().next() == Some(branch_pid))
    {
        return Some(format!("{target} has a squash merge of {branch}"));
    }
    None
}

/// The base branch a branch's integration is measured against, using the same
/// precedence ladder as worktree creation. A branch is never measured against
/// itself, so checking out the base in a removable worktree cannot make that
/// base branch look integrated.
fn integration_target(config: &Config, repo: &str, branch: &str) -> Option<String> {
    let head = git::git_stdout(&["-C", repo, "symbolic-ref", "--quiet", "--short", "HEAD"]);
    let target = git::resolve_base_ref(config, repo, head.trim());
    [
        format!("refs/heads/{target}"),
        format!("refs/remotes/{target}"),
        target,
    ]
    .into_iter()
    .find(|reference| reference.starts_with("refs/") && git::ref_exists(repo, reference))
    .filter(|reference| reference != &format!("refs/heads/{branch}"))
}

fn merge_base(repo: &str, a: &str, b: &str) -> Option<String> {
    let out = git::git_stdout(&["-C", repo, "merge-base", a, b]);
    let oid = out.trim();
    (!oid.is_empty()).then(|| oid.to_string())
}

fn commit_tree(repo: &str, oid: &str) -> Option<String> {
    let spec = format!("{oid}^{{tree}}");
    let out = git::git_stdout(&["-C", repo, "rev-parse", &spec]);
    let tree = out.trim();
    (!tree.is_empty()).then(|| tree.to_string())
}

/// Hash `git <args>`'s diff output through `git patch-id --verbatim`, returning
/// one `<hash> <hash>` line per input patch.
///
/// The diff stream is captured before hashing because `ChildStdout` has no
/// stable `try_clone`, so two children cannot be connected directly here. This
/// is bounded by [`PATCH_ID_SCAN_MAX_COMMITS`] (and patch-id's own output is
/// one short line per patch — well inside a pipe buffer), so neither side can
/// deadlock on a full pipe. Any failure yields `None`: callers treat "no patch
/// ids" as "not integrated".
fn patch_ids_from(repo: &str, args: &[&str], stdin: Option<Vec<u8>>) -> Option<String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut argv: Vec<&str> = vec!["-C", repo];
    argv.extend_from_slice(args);
    // Capture the source's diff stream. With `stdin` data (a rev-list commit
    // list feeding `diff-tree --stdin`) the list is written first: it is
    // small, so write-then-read cannot deadlock.
    let diffs = match stdin {
        None => {
            let output = git::git_output(&argv).ok()?;
            if !output.status.success() {
                return None;
            }
            output.stdout
        }
        Some(data) => {
            let mut child = Command::new("git")
                .env("LC_ALL", "C")
                .args(&argv)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            if child
                .stdin
                .take()
                .is_none_or(|mut pipe| pipe.write_all(&data).is_err())
            {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            let output = child.wait_with_output().ok()?;
            if !output.status.success() {
                return None;
            }
            output.stdout
        }
    };

    let mut piper = Command::new("git")
        .env("LC_ALL", "C")
        .args(["patch-id", "--verbatim"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if piper
        .stdin
        .take()
        .is_none_or(|mut pipe| pipe.write_all(&diffs).is_err())
    {
        let _ = piper.kill();
        let _ = piper.wait();
        return None;
    }
    let mut ids = String::new();
    piper.stdout.as_mut()?.read_to_string(&mut ids).ok()?;
    piper.wait().ok()?.success().then_some(ids)
}

fn risk_is_covered(authorized: Option<RemovalRisk>, current: Option<RemovalRisk>) -> bool {
    let Some(current) = current else {
        return true;
    };
    let Some(authorized) = authorized else {
        return false;
    };
    (!current.dirty || authorized.dirty)
        && (!current.unpublished || authorized.unpublished)
        && (!current.detached || authorized.detached)
}

/// Follow a removal log in a temporary Herdr pane until the worker writes its
/// completion marker, or until the worker is gone without having written one.
pub fn run_progress(args: &[String]) -> Result<()> {
    if args.len() != 1 {
        anyhow::bail!("usage: remove-progress <log>");
    }
    let log = &args[0];
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    progress.set_message("Preparing removal…");
    progress.enable_steady_tick(Duration::from_millis(80));

    let outcome = follow_progress(log, &progress);
    progress.finish_and_clear();
    if let ProgressOutcome::WorkerLost(reason) = outcome {
        tty::err(&format!(
            "{reason}; some worktrees may not have been removed — see {log}"
        ));
        tty::wait_key();
    }
    Ok(())
}

enum ProgressOutcome {
    Done,
    /// The worker is gone without having written its completion marker.
    WorkerLost(&'static str),
}

/// Mirror the worker's log into `progress` until it finishes or disappears.
fn follow_progress(log: &str, progress: &ProgressBar) -> ProgressOutcome {
    let mut shown = 0usize;
    let mut pending = String::new();
    let mut worker = None;
    let mut missing_worker_polls = 0u32;
    let started = Instant::now();

    loop {
        if let Ok(bytes) = std::fs::read(log) {
            if bytes.len() < shown {
                shown = 0;
                pending.clear();
            }
            if bytes.len() > shown {
                pending.push_str(&String::from_utf8_lossy(&bytes[shown..]));
                shown = bytes.len();
                while let Some(newline) = pending.find('\n') {
                    let line: String = pending.drain(..=newline).collect();
                    let line = line.trim_end_matches(['\r', '\n']);
                    if line == PROGRESS_DONE {
                        return ProgressOutcome::Done;
                    }
                    if line.starts_with(PROGRESS_PID) {
                        worker = parse_pid_header(line).or(worker);
                    } else if let Some(update) = line.strip_prefix(PROGRESS_UPDATE) {
                        progress.set_message(update.to_string());
                    } else {
                        progress.println(line);
                    }
                }
            }
        }

        // A worker killed outright (SIGKILL, OOM) never runs its completion
        // guard, so without this the pane would spin until closed by hand.
        match worker {
            Some(pid) if !process_is_running(pid) => {
                missing_worker_polls += 1;
                // Give a worker that exited normally one more poll to have its
                // final marker land on disk before calling it dead.
                if missing_worker_polls > 1 {
                    return ProgressOutcome::WorkerLost("removal worker exited unexpectedly");
                }
            }
            Some(_) => missing_worker_polls = 0,
            None => {
                if started.elapsed() > PROGRESS_PID_TIMEOUT {
                    return ProgressOutcome::WorkerLost("removal worker never started");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn parse_pid_header(line: &str) -> Option<u32> {
    line.strip_prefix(PROGRESS_PID)?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

/// Whether `pid` still exists. Only `ESRCH` proves it is gone; every other
/// failure keeps the follower waiting rather than reporting a false death.
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn report_progress(message: &str) {
    println!("{message}");
    let _ = std::io::stdout().flush();
}

fn report_progress_update(message: &str) {
    println!("{PROGRESS_UPDATE}{message}");
    let _ = std::io::stdout().flush();
}

struct ProgressCompletion;

impl Drop for ProgressCompletion {
    fn drop(&mut self) {
        report_progress(PROGRESS_DONE);
    }
}

/// argv for the detached worker: `remove-bg-batch <repo>` plus one
/// branch/path/head/risk quadruple per target. `decode_batch_args` is the other
/// half of this protocol.
fn encode_batch_args(repo: &str, targets: &[PreparedRemoval]) -> Vec<String> {
    let mut args = vec!["remove-bg-batch".to_string(), repo.to_string()];
    for target in targets {
        args.push(target.branch.clone());
        args.push(target.path.clone());
        args.push(target.head.clone());
        args.push(
            target
                .authorized_risk
                .map_or_else(|| SAFE_CODE.to_string(), RemovalRisk::code),
        );
    }
    args
}

/// Decode the worker's argv tail (everything after the subcommand) into the
/// repo and one entry per target. An unreadable safety code fails just that
/// target — the batch keeps going, but the target is never removed.
fn decode_batch_args(args: &[String]) -> Result<(&str, Vec<Result<PreparedRemoval>>)> {
    if args.len() < 1 + BATCH_FIELDS || !(args.len() - 1).is_multiple_of(BATCH_FIELDS) {
        anyhow::bail!(
            "usage: remove-bg-batch <repo> <branch> <path> <head> <risk> [<branch> <path> <head> <risk> ...]"
        );
    }
    let targets = args[1..]
        .chunks_exact(BATCH_FIELDS)
        .map(|fields| {
            let authorized_risk = if fields[3] == SAFE_CODE {
                None
            } else {
                let Some(risk) = RemovalRisk::from_code(&fields[3]) else {
                    return Err(anyhow::anyhow!(
                        "'{}': invalid safety authorization",
                        fields[0]
                    ));
                };
                Some(risk)
            };
            Ok(PreparedRemoval {
                branch: fields[0].clone(),
                path: fields[1].clone(),
                head: fields[2].clone(),
                authorized_risk,
                // The worker recomputes both from the live repository in
                // `validate_and_delete`; argv carries only what confirmation
                // authorized.
                safety: None,
                kept_by_checkout: None,
            })
        })
        .collect();
    Ok((&args[0], targets))
}

/// Detached background mode for a multi-selection. All removals share one
/// process and produce one summary notification.
pub fn run_background_batch(args: &[String]) -> Result<()> {
    // Set up before anything that can fail, so every exit path closes the
    // progress pane. The follower also needs the pid to tell "still removing"
    // from "worker killed".
    let _completion = ProgressCompletion;
    report_progress(&format!("{PROGRESS_PID}{}", std::process::id()));
    let (repo, decoded) = decode_batch_args(args)?;
    let config = Config::load()?;
    let total = decoded.len();
    let mut removed = 0usize;
    let mut removed_branch = String::new();
    let mut bytes = 0u64;
    let mut failures = Vec::new();

    for (index, decoded) in decoded.into_iter().enumerate() {
        let target = match decoded {
            Ok(target) => target,
            Err(error) => {
                let failure = format!("{error:#}");
                report_progress(&format!("✕ [{}/{}] {failure}", index + 1, total));
                failures.push(failure);
                continue;
            }
        };
        report_progress_update(&format!(
            "[{}/{}] Verifying '{}'…",
            index + 1,
            total,
            target.branch
        ));
        match validate_and_delete(&target, &config, repo) {
            Ok((size, warning)) => {
                removed += 1;
                removed_branch.clone_from(&target.branch);
                bytes += size;
                report_progress(&format!(
                    "✓ [{}/{}] Removed '{}'{}",
                    index + 1,
                    total,
                    target.branch,
                    freed_suffix(size)
                ));
                if let Some(warning) = warning {
                    report_progress(&format!("⚠ '{}': {warning}", target.branch));
                    failures.push(format!("'{}': {warning}", target.branch));
                }
            }
            Err(error) => {
                let failure = format!("'{}': {error:#}", target.branch);
                report_progress(&format!("✕ [{}/{}] {failure}", index + 1, total));
                failures.push(failure);
            }
        }
    }

    if failures.is_empty() {
        let title = if total == 1 {
            "worktree removed"
        } else {
            "worktrees removed"
        };
        let body = if total == 1 {
            format!("removed '{removed_branch}'{}", freed_suffix(bytes))
        } else {
            format!("removed {total} worktrees{}", freed_suffix(bytes))
        };
        herdr::notify(title, &body, "done");
    } else {
        let log = std::env::var("WT_LOG").unwrap_or_default();
        let mut body = format!(
            "removed {removed} of {total}; failed to remove {}",
            failures.join(", ")
        );
        if !log.is_empty() {
            body.push_str(&format!(" — {log}"));
        }
        herdr::notify("worktree removal incomplete", &body, "request");
    }
    Ok(())
}

fn validate_and_delete(
    target: &PreparedRemoval,
    config: &Config,
    repo: &str,
) -> Result<(u64, Option<String>)> {
    let current = inspect_target(
        &RemovalTarget {
            branch: target.branch.clone(),
            path: target.path.clone(),
        },
        config,
        repo,
    )?;
    if current.head != target.head {
        anyhow::bail!("HEAD changed after confirmation; worktree was kept");
    }
    if !risk_is_covered(target.authorized_risk, current.authorized_risk) {
        anyhow::bail!("safety state changed after confirmation; worktree was kept");
    }
    // A confirmed unpublished risk deletes with HEAD-only CAS; otherwise a
    // fresh retention proof (recomputed above, so it reflects the ref state at
    // deletion time) upgrades the delete to a verified transaction.
    let retention_proof = if target.authorized_risk.is_some_and(|risk| risk.unpublished) {
        None
    } else {
        match &current.safety {
            Some(DeletionSafety::Published(proof) | DeletionSafety::Integrated(_, proof)) => {
                Some(proof)
            }
            _ => None,
        }
    };
    perform_delete(
        target,
        config,
        repo,
        retention_proof,
        current.kept_by_checkout.as_deref(),
    )
}

/// Let Git validate and remove the registered worktree while estimating freed
/// disk space from the containing filesystem. Plugin code never erases paths.
fn perform_delete(
    target: &PreparedRemoval,
    config: &Config,
    repo: &str,
    retention_proof: Option<&RetentionProof>,
    kept_by_checkout: Option<&str>,
) -> Result<(u64, Option<String>)> {
    // Keep Git's ordinary checkout occupancy guard continuously present across
    // unregistering the real worktree and deleting the ref. A raw update-ref
    // transaction alone has no knowledge of worktrees.
    let reservation = if config.delete_branch() && target.branch != "(detached)" {
        Some(BranchReservation::acquire(repo, target)?)
    } else {
        None
    };
    let wsid = herdr::worktree_workspace_id(&target.path, repo);

    let mut args = vec!["-C", repo, "worktree", "remove"];
    if target.authorized_risk.is_some_and(|risk| risk.dirty) {
        args.push("--force");
    }
    args.push(&target.path);
    report_progress_update(&format!("Removing '{}'…", target.branch));
    let freed = remove_with_progress(&args, Path::new(&target.path))?;

    if let Some(ws) = wsid {
        herdr::run(&["workspace".into(), "close".into(), ws]);
    }

    if config.delete_branch() && target.branch != "(detached)" {
        let occupants = registered_worktrees(repo)?;
        let sibling = occupants.iter().find(|record| {
            record.branch == target.branch
                && reservation
                    .as_ref()
                    .is_none_or(|guard| record.path != guard.path)
        });
        let kept = if let Some(sibling) = sibling
            .map(|record| record.path.as_str())
            .or(kept_by_checkout)
        {
            Some(format!(
                "worktree removed, but branch '{}' is still checked out at {sibling}, so it was kept",
                target.branch
            ))
        } else if !delete_branch_ref(repo, target, retention_proof) {
            Some(
                "worktree removed, but branch or publication state changed, so the branch was kept"
                    .to_string(),
            )
        } else {
            None
        };
        if let Some(guard) = reservation {
            guard.release()?;
        }
        if let Some(note) = kept {
            return Ok((freed, Some(note)));
        }
    }

    Ok((freed, None))
}

/// A no-checkout, locked Git worktree reserves occupancy, not just a plugin
/// mutex. Normal `switch`/`checkout`/`worktree add` honor it. Explicit force,
/// --ignore-other-worktrees and direct HEAD plumbing bypass Git's protection
/// and are outside this protocol. The target checkout itself must not be
/// concurrently switched/rewritten: pre-reservation ABA operations are not a
/// global Git transaction. HEAD/status revalidation rejects observed changes.
/// No hooks, setup or Herdr run here.
struct BranchReservation {
    repo: String,
    path: String,
    parent: std::path::PathBuf,
    head: String,
    released: bool,
    registered: bool,
}

impl BranchReservation {
    fn acquire(repo: &str, target: &PreparedRemoval) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let parent = std::env::temp_dir().join(format!(
            "herdr-branch-reservation-{}-{nonce}",
            std::process::id()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&parent)?;
        let parent = std::fs::canonicalize(parent)?;
        let path = parent.join("checkout").to_string_lossy().into_owned();
        let mut guard = Self {
            repo: repo.into(),
            path,
            parent,
            head: target.head.clone(),
            released: false,
            registered: false,
        };
        if !git::git_success(&[
            "-C",
            repo,
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            "--lock",
            "--reason",
            "herdr branch deletion reservation",
            &guard.path,
            &target.head,
        ]) {
            guard.registered = registered_worktrees(repo)
                .is_ok_and(|records| records.iter().any(|r| r.path == guard.path));
            anyhow::bail!(
                "could not reserve branch occupancy; worktree and branch kept (reservation {})",
                guard.path
            );
        }
        guard.registered = true;
        if !git::git_success(&[
            "-C",
            &guard.path,
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{}", target.branch),
        ]) {
            anyhow::bail!("could not reserve branch HEAD; worktree and branch kept");
        }
        // Verify the original still holds the pinned branch before removing it.
        let records = registered_worktrees(repo)?;
        if !records
            .iter()
            .any(|r| r.path == target.path && r.branch == target.branch && r.head == target.head)
        {
            guard.cleanup()?;
            anyhow::bail!("worktree changed while reserving branch; kept");
        }
        Ok(guard)
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        if !self.registered {
            if Path::new(&self.path).exists() {
                std::fs::remove_dir(&self.path)?;
            }
            std::fs::remove_dir(&self.parent)?;
            self.released = true;
            return Ok(());
        }
        // Detach first: after a successful deletion the symbolic HEAD is unborn.
        // --no-deref never recreates the branch we just deleted.
        let detached = git::git_success(&[
            "-C",
            &self.path,
            "update-ref",
            "--no-deref",
            "HEAD",
            &self.head,
        ]);
        if !detached
            || !git::git_success(&[
                "-C", &self.repo, "worktree", "remove", "--force", "--force", &self.path,
            ])
        {
            anyhow::bail!("reservation cleanup failed at {} (pinned commit {}); recover with git -C {} update-ref --no-deref HEAD {}, then git -C {} worktree remove --force --force {}", self.path, self.head, self.path, self.head, self.repo, self.path);
        }
        std::fs::remove_dir(&self.parent)?;
        self.released = true;
        Ok(())
    }

    fn release(mut self) -> Result<()> {
        self.cleanup()
    }
}

impl Drop for BranchReservation {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("{error:#}");
        }
    }
}

fn delete_branch_ref(
    repo: &str,
    target: &PreparedRemoval,
    retention_proof: Option<&RetentionProof>,
) -> bool {
    let local_ref = format!("refs/heads/{}", target.branch);
    let Some(proof) = retention_proof else {
        return git::git_success(&["-C", repo, "update-ref", "-d", &local_ref, &target.head]);
    };

    let mut child = match std::process::Command::new("git")
        .env("LC_ALL", "C")
        .args(["-C", repo, "update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let transaction = format!(
        "start\nverify {} {}\ndelete {} {}\nprepare\ncommit\n",
        proof.reference, proof.oid, local_ref, target.head
    );
    if child
        .stdin
        .take()
        .is_none_or(|mut stdin| stdin.write_all(transaction.as_bytes()).is_err())
    {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    child.wait().is_ok_and(|status| status.success())
}

fn remove_with_progress(args: &[&str], worktree_path: &Path) -> Result<u64> {
    let probe = worktree_path
        .parent()
        .filter(|path| path.exists())
        .unwrap_or(worktree_path);
    let initial_available = available_space(probe);
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut last_reported_bytes = 0u64;
    let mut max_freed = 0u64;

    let mut child = std::process::Command::new("git")
        .args(args)
        .spawn()
        .context("starting git worktree remove")?;

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("waiting for git worktree remove")?
        {
            break status;
        }

        if let (Some(initial), Some(current)) = (initial_available, available_space(probe)) {
            max_freed = max_freed.max(current.saturating_sub(initial));
        }
        if last_report.elapsed() >= Duration::from_secs(1)
            || max_freed.saturating_sub(last_reported_bytes) >= 1024 * 1024
        {
            if max_freed > 0 {
                report_progress_update(&format!(
                    "Removing files · ~{} freed · {:.1}s elapsed",
                    human_bytes(max_freed),
                    started.elapsed().as_secs_f64()
                ));
            } else {
                report_progress_update(&format!(
                    "Removing files · {:.1}s elapsed",
                    started.elapsed().as_secs_f64()
                ));
            }
            last_report = Instant::now();
            last_reported_bytes = max_freed;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    if status.success() && initial_available.is_some() {
        // Give filesystems with slightly delayed free-space accounting one last
        // chance to publish the blocks released by Git.
        std::thread::sleep(Duration::from_millis(100));
    }
    if let (Some(initial), Some(current)) = (initial_available, available_space(probe)) {
        max_freed = max_freed.max(current.saturating_sub(initial));
    }
    if !status.success() {
        anyhow::bail!("git worktree remove failed; the branch was kept");
    }
    Ok(max_freed)
}

fn available_space(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    fn into_u64<T: Into<u64>>(value: T) -> u64 {
        value.into()
    }
    Some(into_u64(stats.f_bavail).saturating_mul(into_u64(stats.f_frsize)))
}

fn run_remove_fzf(
    list: &str,
    header: &str,
    footer: &str,
    cur_path: &str,
    refresh_cmd: &str,
) -> Result<Vec<String>> {
    let mut args = remove_fzf_args(header, footer);
    args.push("--bind".into());
    args.push(build_remove_bind(list, cur_path, refresh_cmd));
    let mut child = std::process::Command::new("fzf")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawning fzf")?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(list.as_bytes());
    }
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn remove_fzf_args(header: &str, footer: &str) -> Vec<String> {
    vec![
        "--delimiter=\t".into(),
        "--with-nth=3".into(),
        "--accept-nth=1,2".into(),
        "--id-nth=1,2".into(),
        "--multi".into(),
        "--marker=●".into(),
        "--prompt=remove ❯ ".into(),
        "--header".into(),
        header.to_string(),
        "--footer".into(),
        footer.to_string(),
        "--ansi".into(),
        "--reverse".into(),
        "--info=inline".into(),
        "--border=rounded".into(),
    ]
}

fn build_remove_bind(list: &str, cur_path: &str, refresh_cmd: &str) -> String {
    let load = match model::fzf_line_index(list, cur_path) {
        Some(index) if !cur_path.is_empty() => {
            format!("load:pos({index})+unbind(load)+reload-sync({refresh_cmd})")
        }
        _ => format!("load:unbind(load)+reload-sync({refresh_cmd})"),
    };
    format!("{load},change:first,ctrl-r:reload-sync({refresh_cmd})")
}

fn freed_suffix(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else {
        format!(" (~{} freed)", human_bytes(bytes))
    }
}

/// Human-readable byte size: B, KB, MB, GB.
fn human_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_safety_tag, branch_deletion_safety, branch_integration_reason, branch_publication,
        build_remove_bind, changes_display, decode_batch_args, delete_branch_ref,
        deletion_safety_tag, encode_batch_args, follow_progress, freed_suffix, inspect_target,
        parse_pid_header, parse_targets, perform_delete, process_is_running, removal_risk,
        remove_fzf_args, render_remove_candidates, resolve_registered_paths, risk_is_covered,
        sync_has_unpublished, validate_and_delete, ChangeCounts, DeletionSafety, PreparedRemoval,
        ProgressOutcome, RemovalRisk, RemovalTarget, RetentionProof, SyncKind,
    };
    use crate::config::Config;
    use indicatif::ProgressBar;
    use std::path::Path;
    use std::process::Command;

    fn risk(dirty: bool, unpublished: bool, detached: bool) -> Option<RemovalRisk> {
        Some(RemovalRisk {
            dirty,
            unpublished,
            detached,
        })
    }

    #[test]
    fn preserving_the_branch_only_guards_uncommitted_changes() {
        assert_eq!(removal_risk(false, false, false), None);
        assert_eq!(removal_risk(true, false, false), risk(true, false, false));
    }

    #[test]
    fn deleting_the_branch_also_guards_unpublished_commits() {
        assert!(!sync_has_unpublished(SyncKind::Synced));
        assert!(!sync_has_unpublished(SyncKind::Behind));
        for kind in [
            SyncKind::Ahead,
            SyncKind::Diverged,
            SyncKind::Local,
            SyncKind::Gone,
            SyncKind::Detached,
            SyncKind::Loading,
        ] {
            assert!(sync_has_unpublished(kind), "{}", kind.as_str());
        }
        assert_eq!(removal_risk(false, true, false), risk(false, true, false));
        assert_eq!(removal_risk(true, true, false), risk(true, true, false));
        assert_eq!(removal_risk(false, false, false), None);
    }

    #[test]
    fn detached_worktrees_are_never_marked_safe() {
        assert_eq!(removal_risk(false, false, true), risk(false, false, true));
        assert_eq!(removal_risk(true, false, true), risk(true, false, true));
        // A detached HEAD has no branch, so publication cannot apply.
        assert_eq!(removal_risk(false, true, true), risk(false, false, true));
    }

    #[test]
    fn worker_rejects_new_risks_after_confirmation() {
        assert!(!risk_is_covered(None, risk(true, false, false)));
        assert!(!risk_is_covered(
            risk(true, false, false),
            risk(true, true, false)
        ));
        assert!(risk_is_covered(
            risk(true, true, false),
            risk(true, false, false)
        ));
        assert!(!risk_is_covered(
            risk(true, true, false),
            risk(false, false, true)
        ));
    }

    #[test]
    fn risk_codes_round_trip_and_reject_junk() {
        for flags in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let encoded = risk(flags.0, flags.1, flags.2).unwrap();
            assert_eq!(RemovalRisk::from_code(&encoded.code()), Some(encoded));
        }
        // The wire codes predate the flag struct and must stay readable by both
        // sides of the argv protocol.
        assert_eq!(removal_risk(true, false, false).unwrap().code(), "dirty");
        assert_eq!(
            removal_risk(true, true, false).unwrap().code(),
            "dirty-unpublished"
        );
        assert_eq!(
            removal_risk(true, false, true).unwrap().code(),
            "dirty-detached"
        );
        assert_eq!(RemovalRisk::from_code(""), None);
        assert_eq!(RemovalRisk::from_code("safe"), None);
        assert_eq!(RemovalRisk::from_code("dirty-dirty"), None);
        assert_eq!(RemovalRisk::from_code("dirty-bogus"), None);
    }

    #[test]
    fn risk_labels_stay_within_the_safety_column() {
        assert_eq!(removal_risk(true, false, false).unwrap().label(), "⚠ dirty");
        assert_eq!(
            removal_risk(false, true, false).unwrap().label(),
            "⚠ unpublished"
        );
        assert_eq!(
            removal_risk(true, true, false).unwrap().label(),
            "⚠ dirty + unpub"
        );
        assert_eq!(
            removal_risk(false, false, true).unwrap().label(),
            "⚠ detached"
        );
        assert_eq!(
            removal_risk(true, false, true).unwrap().label(),
            "⚠ dirty + detach"
        );
        for label in [
            removal_risk(true, false, false).unwrap().label(),
            removal_risk(false, true, false).unwrap().label(),
            removal_risk(true, true, false).unwrap().label(),
            removal_risk(false, false, true).unwrap().label(),
            removal_risk(true, false, true).unwrap().label(),
        ] {
            assert!(label.chars().count() <= super::COL_SAFETY, "{label}");
        }
        assert_eq!(
            removal_risk(true, true, false).unwrap().description(),
            "uncommitted changes and unpublished commits"
        );
        assert_eq!(
            removal_risk(true, false, true).unwrap().description(),
            "uncommitted changes and a detached HEAD"
        );
        assert_eq!(
            RemovalRisk::from_code("dirty-unpublished-detached")
                .unwrap()
                .description(),
            "uncommitted changes, unpublished commits and a detached HEAD"
        );
    }

    #[test]
    fn background_batch_argv_round_trips() {
        let targets = vec![
            PreparedRemoval {
                branch: "feature".to_string(),
                path: "/tmp/feature".to_string(),
                head: "abc123".to_string(),
                authorized_risk: None,
                safety: None,
                kept_by_checkout: None,
            },
            PreparedRemoval {
                branch: "(detached)".to_string(),
                path: "/tmp/detached".to_string(),
                head: "def456".to_string(),
                authorized_risk: removal_risk(true, false, true),
                safety: None,
                kept_by_checkout: None,
            },
        ];
        let args = encode_batch_args("/tmp/repo", &targets);
        assert_eq!(args[0], "remove-bg-batch");
        // lib::run strips the subcommand before handing argv to the worker.
        let (repo, decoded) = decode_batch_args(&args[1..]).unwrap();
        assert_eq!(repo, "/tmp/repo");
        assert_eq!(decoded.len(), 2);
        for (target, decoded) in targets.iter().zip(decoded) {
            let decoded = decoded.unwrap();
            assert_eq!(decoded.branch, target.branch);
            assert_eq!(decoded.path, target.path);
            assert_eq!(decoded.head, target.head);
            assert_eq!(decoded.authorized_risk, target.authorized_risk);
        }
    }

    #[test]
    fn background_batch_argv_rejects_malformed_input() {
        assert!(decode_batch_args(&["/tmp/repo".to_string()]).is_err());
        assert!(decode_batch_args(&[
            "/tmp/repo".to_string(),
            "feature".to_string(),
            "/tmp/feature".to_string(),
        ])
        .is_err());
        let (_, decoded) = decode_batch_args(&[
            "/tmp/repo".to_string(),
            "feature".to_string(),
            "/tmp/feature".to_string(),
            "abc123".to_string(),
            "not-a-risk".to_string(),
        ])
        .unwrap();
        assert!(decoded[0].is_err());
    }

    #[test]
    fn progress_pane_detects_a_worker_that_died_without_finishing() {
        assert_eq!(
            parse_pid_header("__HERDR_WORKTREE_REMOVE_PID__4821"),
            Some(4821)
        );
        assert_eq!(parse_pid_header("__HERDR_WORKTREE_REMOVE_PID__0"), None);
        assert_eq!(parse_pid_header("Removing 'feature'…"), None);

        assert!(process_is_running(std::process::id()));
        assert!(!process_is_running(dead_pid()));
    }

    #[test]
    fn progress_pane_stops_following_a_dead_worker_and_keeps_following_a_live_one() {
        let root = unique_root("progress-follow");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("remove.log");
        std::fs::write(
            &log,
            format!(
                "{}{}\n{}Removing 'feature'…\n",
                super::PROGRESS_PID,
                dead_pid(),
                super::PROGRESS_UPDATE
            ),
        )
        .unwrap();
        let outcome = follow_progress(&log.to_string_lossy(), &ProgressBar::hidden());
        assert!(matches!(outcome, ProgressOutcome::WorkerLost(_)));

        // A live worker that reports completion still ends the pane normally.
        std::fs::write(
            &log,
            format!(
                "{}{}\n{}\n",
                super::PROGRESS_PID,
                std::process::id(),
                super::PROGRESS_DONE
            ),
        )
        .unwrap();
        let outcome = follow_progress(&log.to_string_lossy(), &ProgressBar::hidden());
        assert!(matches!(outcome, ProgressOutcome::Done));
        std::fs::remove_dir_all(root).ok();
    }

    /// A pid that is guaranteed to be gone: our own child, already reaped.
    fn dead_pid() -> u32 {
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn unavailable_disk_delta_is_not_rendered_as_zero_bytes() {
        assert_eq!(freed_suffix(0), "");
        assert_eq!(freed_suffix(1024), " (~1.0 KB freed)");
    }

    #[test]
    fn parses_every_fzf_multi_selection() {
        let targets = parse_targets(&["one\t/tmp/one".to_string(), "two\t/tmp/two".to_string()]);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].branch, "one");
        assert_eq!(targets[1].path, "/tmp/two");
    }

    #[test]
    fn removal_changes_distinguish_untracked_files() {
        let parsed = crate::status::parse_porcelain(b"M  staged\n M unstaged\n?? untracked\n");
        assert_eq!(
            (parsed.staged, parsed.unstaged, parsed.untracked),
            (1, 1, 1)
        );
        assert_eq!(changes_display(ChangeCounts::default()), "clean");
        assert_eq!(
            changes_display(ChangeCounts {
                untracked: 2,
                ..ChangeCounts::default()
            }),
            "untracked"
        );
        assert_eq!(
            changes_display(ChangeCounts {
                staged: 1,
                unstaged: 2,
                untracked: 3,
            }),
            "+1 ~2 ?3"
        );
    }

    #[test]
    fn remove_binding_enriches_without_changing_item_identity() {
        let args = remove_fzf_args("header", "footer");
        assert!(args.iter().any(|arg| arg == "--multi"));
        assert!(args.iter().any(|arg| arg == "--id-nth=1,2"));
        let list = "one\t/tmp/one\tchecking\ntwo\t/tmp/two\tchecking";
        let bind = build_remove_bind(list, "/tmp/two", "picker remove-list");
        assert!(bind.starts_with("load:pos(2)+unbind(load)+reload-sync(picker remove-list)"));
        assert!(bind.contains("ctrl-r:reload-sync(picker remove-list)"));
    }

    #[test]
    fn enrichment_keeps_identity_and_overrides_hidden_untracked_config() {
        let root = std::env::temp_dir().join(format!(
            "herdr-remove-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("seed"), "seed").unwrap();
        git(&repo, &["add", "seed"]);
        git(&repo, &["commit", "-qm", "seed"]);
        git(&repo, &["branch", "feature"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "feature",
            ],
        );
        git(&repo, &["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(worktree.join("untracked"), "unsafe").unwrap();

        let config = Config::default();
        let state = root.join("state");
        let repo = repo.to_str().unwrap();
        let initial = render_remove_candidates(repo, &config, &state, false);
        let enriched = render_remove_candidates(repo, &config, &state, true);
        let identity = |row: &str| {
            row.split('\t')
                .take(2)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(identity(&initial), identity(&enriched));
        assert!(initial.contains("… checking"));
        assert!(enriched.contains("⚠ dirty"));
        assert!(enriched.contains("untracked"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn worker_rejects_untracked_changes_added_after_confirmation() {
        let root = unique_root("safety-recheck");
        let repo = root.join("repo");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["branch", "feature"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "feature",
            ],
        );
        let target = RemovalTarget {
            branch: "feature".to_string(),
            path: std::fs::canonicalize(&worktree)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        };
        let prepared = inspect_target(&target, &Config::default(), repo.to_str().unwrap()).unwrap();
        assert_eq!(prepared.authorized_risk, None);
        std::fs::write(worktree.join("late-untracked"), "unsafe").unwrap();

        assert!(
            validate_and_delete(&prepared, &Config::default(), repo.to_str().unwrap()).is_err()
        );
        assert!(worktree.is_dir());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_differently_spelled_path_still_names_the_registered_worktree() {
        let root = unique_root("path-spelling");
        let repo = root.join("repo");
        let worktree = repo.join(".worktrees/feature");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let registered = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let repo_str = repo.to_str().unwrap();

        // The same directory reached through `..`, the way a shell-relative
        // path reaches it once canonicalized.
        let detoured = repo
            .join("seed/../.worktrees/feature")
            .to_string_lossy()
            .into_owned();
        let target = RemovalTarget {
            branch: "feature".to_string(),
            path: detoured.clone(),
        };
        let prepared = inspect_target(&target, &Config::default(), repo_str).unwrap();
        // Inspection succeeds, and hands git's own spelling to the deletion.
        assert_eq!(prepared.path, registered);

        let resolved = resolve_registered_paths(std::slice::from_ref(&target), repo_str);
        assert_eq!(resolved[0].path, registered);

        // An unregistered path is left as typed, and still reported as such.
        let stranger = RemovalTarget {
            branch: "feature".to_string(),
            path: repo.join(".worktrees/other").to_string_lossy().into_owned(),
        };
        assert_eq!(
            resolve_registered_paths(std::slice::from_ref(&stranger), repo_str)[0].path,
            stranger.path
        );
        let error = inspect_target(&stranger, &Config::default(), repo_str)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("is no longer a registered worktree"),
            "{error}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn safety_tags_map_from_deletion_verdicts() {
        assert_eq!(
            deletion_safety_tag(&DeletionSafety::Published(RetentionProof {
                reference: "refs/remotes/origin/main".to_string(),
                oid: "abc".to_string(),
            })),
            Some("pushed")
        );
        assert_eq!(
            deletion_safety_tag(&DeletionSafety::Integrated(
                "feature content matches main".to_string(),
                RetentionProof {
                    reference: "refs/heads/main".into(),
                    oid: "abc".into()
                }
            )),
            Some("merged")
        );
        // The ⚠ label already carries this warning; a tag would repeat it.
        assert_eq!(deletion_safety_tag(&DeletionSafety::Unpublished), None);

        let cell = render_cell();
        assert_eq!(
            append_safety_tag(&cell, Some("merged")),
            format!("{cell}\x1b[2m ·merged\x1b[0m")
        );
        assert_eq!(append_safety_tag(&cell, None), cell);
    }

    fn render_cell() -> String {
        "\x1b[32m✓ safe           \x1b[0m".to_string()
    }

    #[test]
    fn enrichment_tags_why_a_branch_is_deletable_without_touching_identity() {
        let root = std::env::temp_dir().join(format!(
            "herdr-remove-tag-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("seed"), "seed").unwrap();
        git(&repo, &["add", "seed"]);
        git(&repo, &["commit", "-qm", "seed"]);
        // Same commit as main and no upstream: the integration probe proves it
        // deletable via check 1.
        git(&repo, &["branch", "feature"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "feature",
            ],
        );

        let config = delete_branch_config();
        let state = root.join("state");
        let repo_str = repo.to_str().unwrap();
        let enriched = render_remove_candidates(repo_str, &config, &state, true);
        assert!(enriched.contains("\x1b[2m ·merged\x1b[0m"), "{enriched}");

        // The dimmed tag lives inside the final column: the identity columns
        // fzf accepts back must be exactly branch and path.
        for line in enriched.lines() {
            let mut fields = line.split('\t');
            assert_eq!(fields.next(), Some("feature"));
            assert_eq!(
                fields.next(),
                Some(worktree.canonicalize().unwrap().to_string_lossy().as_ref())
            );
            assert_eq!(fields.count(), 1, "exactly one display column remains");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn publication_transaction_keeps_branch_when_upstream_moves() {
        let root = unique_root("publication-proof");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["branch", "feature"]);
        let feature_head = git_output(&repo, &["rev-parse", "feature"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "https://example.invalid/repo.git",
            ],
        );
        git(
            &repo,
            &["update-ref", "refs/remotes/upstream/feature", &feature_head],
        );
        git(
            &repo,
            &["branch", "--set-upstream-to=upstream/feature", "feature"],
        );
        let proof = branch_publication(repo.to_str().unwrap(), "feature").unwrap();
        assert_eq!(
            proof.as_ref().map(|proof| proof.reference.as_str()),
            Some("refs/remotes/upstream/feature")
        );

        std::fs::write(repo.join("later"), "later").unwrap();
        git(&repo, &["add", "later"]);
        git(&repo, &["commit", "-qm", "later"]);
        let new_remote = git_output(&repo, &["rev-parse", "main"]);
        git(
            &repo,
            &["update-ref", "refs/remotes/upstream/feature", &new_remote],
        );
        let target = PreparedRemoval {
            branch: "feature".to_string(),
            path: String::new(),
            head: feature_head.clone(),
            authorized_risk: None,
            safety: None,
            kept_by_checkout: None,
        };
        assert!(!delete_branch_ref(
            repo.to_str().unwrap(),
            &target,
            proof.as_ref()
        ));
        assert_eq!(git_output(&repo, &["rev-parse", "feature"]), feature_head);
        std::fs::remove_dir_all(root).ok();
    }

    /// A repo whose `main` carries one seed commit; returns (root, repo).
    fn repo_with_main(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = unique_root(label);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        (root, repo)
    }

    fn delete_branch_config() -> Config {
        toml::from_str("[remove]\ndelete-branch = true").unwrap()
    }

    #[test]
    fn upstream_proof_ranks_above_integration_probes() {
        let (root, repo) = repo_with_main("safety-published");
        git(&repo, &["branch", "feature"]);
        // No upstream at all: the integration probes could still pass (same
        // commit as main), but the published verdict must win when one exists.
        let config = delete_branch_config();
        let repo_str = repo.to_str().unwrap();
        assert_eq!(
            branch_deletion_safety(&config, repo_str, "feature").unwrap(),
            DeletionSafety::Integrated(
                "feature is at the same commit as refs/heads/main".to_string(),
                RetentionProof {
                    reference: "refs/heads/main".into(),
                    oid: git_output(&repo, &["rev-parse", "main"])
                }
            )
        );
        git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/feature",
                &git_output(&repo, &["rev-parse", "feature"]),
            ],
        );
        match branch_deletion_safety(&config, repo_str, "feature").unwrap() {
            DeletionSafety::Published(proof) => {
                assert_eq!(proof.reference, "refs/remotes/origin/feature");
            }
            other => panic!("expected Published, got {other:?}"),
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn squash_merged_branch_is_integrated_via_simulated_merge() {
        let (root, repo) = repo_with_main("safety-squash");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("work"), "done").unwrap();
        git(&repo, &["add", "work"]);
        git(&repo, &["commit", "-qm", "feature work"]);
        // Land the same change on main as a single squash commit, then move
        // main forward with unrelated changes so the trees differ and only
        // the simulated merge can prove integration.
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["merge", "--squash", "-q", "feature"]);
        git(&repo, &["commit", "-qm", "squash feature"]);
        std::fs::write(repo.join("other"), "unrelated").unwrap();
        git(&repo, &["add", "other"]);
        git(&repo, &["commit", "-qm", "unrelated work"]);

        let config = delete_branch_config();
        let reason = branch_integration_reason(&config, repo.to_str().unwrap(), "feature")
            .expect("a squash-merged branch must be recognized as integrated");
        assert!(reason.contains("adds nothing"), "{reason}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rebased_onto_identical_content_is_integrated_via_tree_match() {
        let (root, repo) = repo_with_main("safety-tree");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("seed"), "rewritten").unwrap();
        git(&repo, &["add", "seed"]);
        git(&repo, &["commit", "-qm", "rewrite"]);
        // Main independently produces the exact same content.
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["merge", "--squash", "-q", "feature"]);
        git(&repo, &["commit", "-qm", "same content on main"]);

        let config = delete_branch_config();
        let reason = branch_integration_reason(&config, repo.to_str().unwrap(), "feature")
            .expect("identical trees must count as integrated");
        assert!(reason.contains("content matches"), "{reason}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn squash_merge_conflicting_with_later_base_edits_is_detected_by_patch_id() {
        let (root, repo) = repo_with_main("safety-patch-id");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("seed"), "branch work").unwrap();
        git(&repo, &["commit", "-qam", "feature work"]);
        // Squash the branch onto main, then have main touch the SAME file.
        // The simulated merge now conflicts, so only the patch-id fallback
        // can prove integration; without it this branch reads as unmerged.
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["merge", "--squash", "-q", "feature"]);
        git(&repo, &["commit", "-qm", "squash feature"]);
        std::fs::write(repo.join("seed"), "later edit\n").unwrap();
        git(&repo, &["commit", "-qam", "main edits the same file"]);

        let config = delete_branch_config();
        let reason = branch_integration_reason(&config, repo.to_str().unwrap(), "feature")
            .expect("a conflicted squash merge must still count as integrated");
        assert!(reason.contains("squash merge"), "{reason}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn patch_id_match_requires_the_whole_branch_diff_on_the_target() {
        let (root, repo) = repo_with_main("safety-patch-id-miss");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("seed"), "branch work").unwrap();
        std::fs::write(repo.join("extra"), "more").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "feature work"]);
        // Main independently changes the same file but never receives the
        // branch's full diff — a partial overlap must not read as integrated.
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("seed"), "different work").unwrap();
        git(&repo, &["commit", "-qam", "unrelated change to seed"]);

        let config = delete_branch_config();
        assert_eq!(
            branch_integration_reason(&config, repo.to_str().unwrap(), "feature"),
            None
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unmerged_branch_is_not_integrated() {
        let (root, repo) = repo_with_main("safety-unmerged");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("work"), "undone").unwrap();
        git(&repo, &["add", "work"]);
        git(&repo, &["commit", "-qm", "unmerged work"]);
        git(&repo, &["checkout", "-q", "main"]);

        let config = delete_branch_config();
        assert_eq!(
            branch_integration_reason(&config, repo.to_str().unwrap(), "feature"),
            None
        );
        assert_eq!(
            branch_deletion_safety(&config, repo.to_str().unwrap(), "feature").unwrap(),
            DeletionSafety::Unpublished
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_branch_is_never_measured_against_itself() {
        let (root, repo) = repo_with_main("safety-self");
        let config = delete_branch_config();
        // Removing a worktree that holds the base branch itself must not make
        // that base look integrated; the sibling guard is what protects it.
        assert_eq!(
            branch_integration_reason(&config, repo.to_str().unwrap(), "main"),
            None
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_sibling_checkout_keeps_the_branch_alive() {
        let (root, repo) = repo_with_main("safety-sibling");
        git(&repo, &["branch", "feature"]);
        let first = root.join("first");
        let second = root.join("second");
        git(
            &repo,
            &["worktree", "add", "-q", first.to_str().unwrap(), "feature"],
        );
        // Git refuses the same branch in two worktrees unless forced; that is
        // exactly the topology this guard exists for.
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--force",
                "-q",
                second.to_str().unwrap(),
                "feature",
            ],
        );

        let config = delete_branch_config();
        let target = RemovalTarget {
            branch: "feature".to_string(),
            path: std::fs::canonicalize(&first)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        };
        let prepared = inspect_target(&target, &config, repo.to_str().unwrap()).unwrap();
        assert_eq!(
            prepared.kept_by_checkout.as_deref(),
            Some(
                std::fs::canonicalize(&second)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
        // The worktree itself is still removable — only the ref is protected.
        assert_eq!(prepared.authorized_risk, None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn removal_with_a_sibling_checkout_keeps_the_branch() {
        let (root, repo) = repo_with_main("safety-sibling-delete");
        git(&repo, &["branch", "feature"]);
        let first = root.join("first");
        let second = root.join("second");
        git(
            &repo,
            &["worktree", "add", "-q", first.to_str().unwrap(), "feature"],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--force",
                "-q",
                second.to_str().unwrap(),
                "feature",
            ],
        );

        let config = delete_branch_config();
        let target = PreparedRemoval {
            branch: "feature".to_string(),
            path: std::fs::canonicalize(&first)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            head: git_output(&repo, &["rev-parse", "feature"]),
            authorized_risk: None,
            safety: None,
            kept_by_checkout: None,
        };
        let (freed, note) = perform_delete(
            &target,
            &config,
            repo.to_str().unwrap(),
            None,
            Some(
                std::fs::canonicalize(&second)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref(),
            ),
        )
        .unwrap();
        let _ = freed;
        let note = note.expect("the kept branch must be reported");
        assert!(note.contains("still checked out"), "{note}");
        assert!(!first.exists());
        // The ref survives for the surviving checkout.
        assert_eq!(
            crate::git::ref_oid(repo.to_str().unwrap(), "refs/heads/feature"),
            Some(git_output(&repo, &["rev-parse", "feature"]))
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn integrated_branch_deletes_without_upstream_proof() {
        let (root, repo) = repo_with_main("safety-integrated-delete");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("work"), "done").unwrap();
        git(&repo, &["add", "work"]);
        git(&repo, &["commit", "-qm", "feature work"]);
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["merge", "--squash", "-q", "feature"]);
        git(&repo, &["commit", "-qm", "squash feature"]);
        let feature_head = git_output(&repo, &["rev-parse", "feature"]);

        let target = PreparedRemoval {
            branch: "feature".to_string(),
            path: String::new(),
            head: feature_head.clone(),
            authorized_risk: None,
            safety: None,
            kept_by_checkout: None,
        };
        // HEAD-only CAS: no proof, and the branch must be gone afterwards.
        assert!(delete_branch_ref(repo.to_str().unwrap(), &target, None));
        assert_eq!(
            crate::git::ref_oid(repo.to_str().unwrap(), "refs/heads/feature"),
            None
        );
        std::fs::remove_dir_all(root).ok();
    }

    fn unique_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-remove-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn init_repo(repo: &Path) {
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("seed"), "seed").unwrap();
        git(repo, &["add", "seed"]);
        git(repo, &["commit", "-qm", "seed"]);
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
