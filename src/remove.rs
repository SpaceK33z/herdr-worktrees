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
    publication_proof: Option<PublicationProof>,
}

#[derive(Debug, Clone)]
struct PublicationProof {
    reference: String,
    oid: String,
}

struct RemovalInspection {
    prepared: PreparedRemoval,
    changes: ChangeCounts,
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
    delete_worktrees(&targets, &config, &repo)
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
        .filter(|worktree| worktree.path != repo)
        .collect();
    let inspections = inspect.then(|| inspect_candidates(&removable, config));
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
                render_safety(inspection.prepared.authorized_risk)
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
) -> Vec<Result<RemovalInspection>> {
    inspect_in_parallel(worktrees, |worktree| inspect_candidate(worktree, config))
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

fn inspect_candidate(worktree: &model::Worktree, config: &Config) -> Result<RemovalInspection> {
    let changes = inspect_changes(&worktree.path)?;
    let detached = worktree.branch.is_empty();
    let unpublished = config.delete_branch() && !detached && sync_has_unpublished(worktree.sync_kind);
    Ok(RemovalInspection {
        prepared: PreparedRemoval {
            branch: branch_name(worktree),
            path: worktree.path.clone(),
            head: worktree.head.clone(),
            authorized_risk: removal_risk(changes.dirty(), unpublished, detached),
            publication_proof: None,
        },
        changes,
    })
}

/// The `remove --target <branch> <path>` one-off delete used by the picker's
/// ctrl-d. The row's cached kind and changes used to be passed along too; the
/// removal re-inspects both, so they are no longer part of the protocol.
pub fn run_target(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) != Some("--target") || args.len() != 3 {
        anyhow::bail!("usage: remove --target <branch> <path>");
    }
    let branch = &args[1];
    let path = &args[2];

    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    if path.as_str() == repo.as_str() {
        tty::err("the main checkout can't be removed");
        return Ok(());
    }
    let config = Config::load()?;
    delete_worktree(branch, path, &config, &repo)
}

/// Confirm, remove the checkout, optionally delete the branch, and close the
/// Herdr workspace that was open for it.
pub fn delete_worktree(branch: &str, path: &str, config: &Config, repo: &str) -> Result<()> {
    delete_worktrees(
        &[RemovalTarget {
            branch: branch.to_string(),
            path: path.to_string(),
        }],
        config,
        repo,
    )
}

fn delete_worktrees(targets: &[RemovalTarget], config: &Config, repo: &str) -> Result<()> {
    if targets.iter().any(|target| target.path == repo) {
        tty::err("the main checkout can't be removed");
        return Ok(());
    }

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
    let require_force = !risks.is_empty() && !config.force();
    let prompt = removal_prompt(targets, &risks, require_force);
    if !tty::confirm(&prompt, require_force) {
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
            Some(workspace) => herdr::open_tab_pane(Some(&workspace), &target.path, &label),
            None => herdr::open_worktree_pane(
                herdr::root_workspace(repo).as_deref(),
                repo,
                &target.path,
                &label,
            ),
        }
    } else {
        let cwd = if targets.len() == 1 {
            targets[0].path.as_str()
        } else {
            repo
        };
        herdr::open_tab_pane(herdr::current_workspace().as_deref(), cwd, &label)
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

fn removal_prompt(targets: &[RemovalTarget], risks: &[RemovalRisk], require_force: bool) -> String {
    if targets.len() == 1 {
        let branch = &targets[0].branch;
        if require_force {
            return format!(
                "  ⚠ '{branch}' has {} — ctrl-x to remove anyway, any other key to cancel",
                risks[0].description()
            );
        }
        return format!("  remove '{branch}'? enter to confirm, any other key to cancel");
    }

    if require_force {
        format!(
            "  ⚠ {} of {} selected worktrees are not safe to remove — ctrl-x to remove all anyway, any other key to cancel",
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
        .find(|record| record.path == target.path)
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

    let changes = inspect_changes(&target.path)?;
    let detached = record.branch.is_empty();
    let (unpublished, publication_proof) = if config.delete_branch() && !detached {
        branch_publication(repo, &record.branch)?
    } else {
        (false, None)
    };

    Ok(PreparedRemoval {
        branch: target.branch.clone(),
        path: target.path.clone(),
        head: record.head.clone(),
        authorized_risk: removal_risk(changes.dirty(), unpublished, detached),
        publication_proof,
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

fn branch_publication(repo: &str, branch: &str) -> Result<(bool, Option<PublicationProof>)> {
    let Some(upstream) = git::branch_upstream(repo, branch) else {
        // Failure to resolve an upstream is conservative: keeping the branch is
        // safe, deleting it needs explicit force confirmation.
        return Ok((true, None));
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
        return Ok((true, None));
    }

    // Compare against the captured object, not the mutable ref name. The same
    // OID is verified in the update-ref transaction before branch deletion.
    let range = format!("{oid}..{branch}");
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
        Ok((true, None))
    } else {
        Ok((
            false,
            Some(PublicationProof {
                reference: upstream,
                oid: oid.to_string(),
            }),
        ))
    }
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
                publication_proof: None,
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
    let publication_proof = if target.authorized_risk.is_some_and(|risk| risk.unpublished) {
        None
    } else {
        current.publication_proof.as_ref()
    };
    perform_delete(target, config, repo, publication_proof)
}

/// Let Git validate and remove the registered worktree while estimating freed
/// disk space from the containing filesystem. Plugin code never erases paths.
fn perform_delete(
    target: &PreparedRemoval,
    config: &Config,
    repo: &str,
    publication_proof: Option<&PublicationProof>,
) -> Result<(u64, Option<String>)> {
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

    if config.delete_branch()
        && target.branch != "(detached)"
        && !delete_branch_ref(repo, target, publication_proof)
    {
        return Ok((
            freed,
            Some(
                "worktree removed, but branch or publication state changed, so the branch was kept"
                    .to_string(),
            ),
        ));
    }

    Ok((freed, None))
}

fn delete_branch_ref(
    repo: &str,
    target: &PreparedRemoval,
    publication_proof: Option<&PublicationProof>,
) -> bool {
    let local_ref = format!("refs/heads/{}", target.branch);
    let Some(proof) = publication_proof else {
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
        branch_publication, build_remove_bind, changes_display, decode_batch_args,
        delete_branch_ref, encode_batch_args, follow_progress, freed_suffix, inspect_target,
        parse_pid_header, parse_targets, process_is_running, removal_risk, remove_fzf_args,
        render_remove_candidates, risk_is_covered, sync_has_unpublished, validate_and_delete,
        ChangeCounts, PreparedRemoval, ProgressOutcome, RemovalRisk, RemovalTarget, SyncKind,
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
                publication_proof: None,
            },
            PreparedRemoval {
                branch: "(detached)".to_string(),
                path: "/tmp/detached".to_string(),
                head: "def456".to_string(),
                authorized_risk: removal_risk(true, false, true),
                publication_proof: None,
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
        let (unpublished, proof) = branch_publication(repo.to_str().unwrap(), "feature").unwrap();
        assert!(!unpublished);
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
            publication_proof: None,
        };
        assert!(!delete_branch_ref(
            repo.to_str().unwrap(),
            &target,
            proof.as_ref()
        ));
        assert_eq!(git_output(&repo, &["rev-parse", "feature"]), feature_head);
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
