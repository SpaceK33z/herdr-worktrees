//! Worktree metadata engine: discover worktrees and branches, compute each one
//! in parallel, and render `--json` / `--fzf` / `--header` output.

use crate::config::{branch_short_name, Config};
use crate::git;
use crate::pr::{self, PrInfo};
use crate::render;
use crate::status;
use crate::util;
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: String,
    pub branch: String,
    pub branch_short: String,
    pub head: String,
    pub detached: bool,
    pub is_main: bool,
    pub is_base: bool,
    pub when: String,
    pub when_ts: i64,
    pub staged: u32,
    pub unstaged: u32,
    pub changes: String,
    pub dirty: bool,
    pub sync: String,
    pub sync_kind: String,
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub review: String,
    pub conflict: bool,
    pub created_ts: f64,
}

#[derive(Debug, Serialize)]
pub struct Engine {
    pub repo_path: String,
    pub repo_name: String,
    pub base: String,
    pub base_short: String,
    #[serde(skip)]
    pub prefix: String,
    pub worktrees: Vec<Worktree>,
    /// Local branches that are not currently checked out in a worktree.
    pub branches: Vec<Worktree>,
    /// `origin` remote-tracking branches without a corresponding local branch.
    pub remote_branches: Vec<Worktree>,
}

pub fn state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    PathBuf::from("/tmp/herdr-worktrees-state")
}

#[derive(Default)]
pub(crate) struct RawWorktree {
    pub path: String,
    pub head: String,
    pub branch: String,
}

#[derive(Debug, Clone, Default)]
struct RefMetadata {
    head: String,
    when_ts: i64,
    upstream: String,
    tracking: String,
}

#[derive(Default)]
struct RefSnapshot {
    local: BTreeMap<String, RefMetadata>,
    origin: BTreeMap<String, RefMetadata>,
    remote_heads: HashMap<String, String>,
    origin_head: Option<String>,
}

#[derive(Clone, Copy)]
struct ComputeOptions {
    inspect_worktrees: bool,
    fetch_prs: bool,
    exact_sync: bool,
    include_branches: bool,
}

impl ComputeOptions {
    const FULL: Self = Self {
        inspect_worktrees: true,
        fetch_prs: true,
        exact_sync: true,
        include_branches: true,
    };
    const PICKER_INITIAL: Self = Self {
        inspect_worktrees: false,
        fetch_prs: false,
        exact_sync: false,
        include_branches: true,
    };
    const REMOVE_INITIAL: Self = Self {
        inspect_worktrees: false,
        fetch_prs: false,
        exact_sync: false,
        include_branches: false,
    };
    const REMOVE_SNAPSHOT: Self = Self {
        inspect_worktrees: false,
        fetch_prs: false,
        exact_sync: true,
        include_branches: false,
    };
}

/// Parse `git worktree list --porcelain` into (path, head, branch) records.
/// `branch` is the short name (the `refs/heads/` prefix is stripped); it is
/// empty for detached checkouts.
pub(crate) fn parse_worktree_list(porcelain: &str) -> Vec<RawWorktree> {
    let mut records = Vec::new();
    let mut cur = RawWorktree::default();
    for line in porcelain.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur.path = p.to_string();
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            cur.head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            cur.branch = b.to_string();
        } else if line.is_empty() && !cur.path.is_empty() {
            records.push(std::mem::take(&mut cur));
        }
    }
    if !cur.path.is_empty() {
        records.push(cur);
    }
    records
}

/// Compute the full engine for `repo`: every worktree, newest first.
pub fn compute_all(repo: &str, config: &Config, state_dir: &Path, _use_cache: bool) -> Engine {
    compute(repo, config, state_dir, ComputeOptions::FULL)
}

/// Compute the local metadata needed to draw the first picker frame.
/// Worktree scans and GitHub requests are filled in by fzf's background reload.
pub fn compute_picker_initial(repo: &str, config: &Config, state_dir: &Path) -> Engine {
    compute(repo, config, state_dir, ComputeOptions::PICKER_INITIAL)
}

/// Compute the first remove-picker frame without scanning worktree files.
pub fn compute_remove_initial(repo: &str, config: &Config, state_dir: &Path) -> Engine {
    compute(repo, config, state_dir, ComputeOptions::REMOVE_INITIAL)
}

/// Compute exact worktree sync metadata without scanning files or PRs.
/// The remove picker combines this snapshot with its own untracked-aware checks.
pub fn compute_remove_snapshot(repo: &str, config: &Config, state_dir: &Path) -> Engine {
    compute(repo, config, state_dir, ComputeOptions::REMOVE_SNAPSHOT)
}

fn compute(repo: &str, config: &Config, state_dir: &Path, options: ComputeOptions) -> Engine {
    let porcelain = git::git_stdout(&["-C", repo, "worktree", "list", "--porcelain"]);
    let records = parse_worktree_list(&porcelain);
    let refs = ref_snapshot(repo, options.exact_sync);

    let current_branch = git::current_branch();
    let base = resolve_base_ref(config, &refs, &current_branch);
    let base_short = util::strip_remote(&base);
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let repo_name = Path::new(repo)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fetch_prs = options.fetch_prs && config.github_prs() && git::has_github_remote(repo);

    let checked_out: std::collections::HashSet<&str> = records
        .iter()
        .filter(|record| !record.branch.is_empty())
        .map(|record| record.branch.as_str())
        .collect();
    let branch_records: Vec<RawWorktree> = if options.include_branches {
        refs.local
            .iter()
            .filter(|(branch, _)| !checked_out.contains(branch.as_str()))
            .map(|(branch, metadata)| RawWorktree {
                path: repo.to_string(),
                head: metadata.head.clone(),
                branch: branch.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let remote_records: Vec<(&String, &RefMetadata)> = if options.include_branches {
        refs.origin
            .iter()
            .filter(|(branch, _)| !refs.local.contains_key(branch.as_str()))
            .collect()
    } else {
        Vec::new()
    };

    let pr_heads: Vec<_> = if fetch_prs {
        refs.local
            .iter()
            .map(|(branch, metadata)| (branch.clone(), metadata.head.clone()))
            .chain(
                remote_records
                    .iter()
                    .map(|(branch, metadata)| ((*branch).clone(), metadata.head.clone())),
            )
            .filter(|(branch, _)| branch.as_str() != base_short)
            .collect()
    } else {
        Vec::new()
    };
    let pr_handle = if fetch_prs {
        let repo = repo.to_string();
        let state_dir = state_dir.to_path_buf();
        Some(std::thread::spawn(move || {
            pr::fetch_many(&pr_heads, &repo, &state_dir)
        }))
    } else {
        None
    };

    let worktrees: Vec<Worktree> = if options.inspect_worktrees && !records.is_empty() {
        let workers = std::thread::available_parallelism()
            .map(|count| count.get().saturating_mul(4))
            .unwrap_or(8)
            .clamp(1, 64)
            .min(records.len());
        let chunk_size = records.len().div_ceil(workers);
        std::thread::scope(|scope| {
            records
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(|| {
                        chunk
                            .iter()
                            .map(|record| {
                                compute_one(
                                    record,
                                    true,
                                    true,
                                    repo,
                                    &base_short,
                                    &prefix,
                                    refs.local.get(&record.branch),
                                    &refs.remote_heads,
                                    options.exact_sync,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .flatten()
                .collect()
        })
    } else {
        records
            .iter()
            .map(|record| {
                compute_one(
                    record,
                    true,
                    false,
                    repo,
                    &base_short,
                    &prefix,
                    refs.local.get(&record.branch),
                    &refs.remote_heads,
                    options.exact_sync,
                )
            })
            .collect()
    };

    let branches: Vec<Worktree> = branch_records
        .iter()
        .map(|record| {
            compute_one(
                record,
                false,
                false,
                repo,
                &base_short,
                &prefix,
                refs.local.get(&record.branch),
                &refs.remote_heads,
                options.exact_sync,
            )
        })
        .collect();
    let remote_branches: Vec<Worktree> = remote_records
        .iter()
        .map(|(branch, metadata)| compute_remote_candidate(branch, metadata, &base_short, &prefix))
        .collect();

    let prs = pr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let mut worktrees = worktrees;
    let mut branches = branches;
    let mut remote_branches = remote_branches;
    for worktree in worktrees.iter_mut().chain(branches.iter_mut()) {
        if !worktree.detached && !worktree.is_base {
            apply_pr(worktree, prs.get(&worktree.branch));
        }
    }
    for remote in &mut remote_branches {
        if !remote.is_base {
            let local = origin_local_branch(&remote.branch)
                .expect("remote candidates always contain an origin branch");
            apply_pr(remote, prs.get(local));
        }
    }
    worktrees.sort_by(|a, b| {
        b.created_ts
            .partial_cmp(&a.created_ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    branches.sort_by(|a, b| b.when_ts.cmp(&a.when_ts));
    remote_branches.sort_by(|a, b| b.when_ts.cmp(&a.when_ts));

    Engine {
        repo_path: repo.to_string(),
        repo_name,
        base,
        base_short,
        prefix,
        worktrees,
        branches,
        remote_branches,
    }
}

fn ref_snapshot(repo: &str, exact_sync: bool) -> RefSnapshot {
    const FULL_FORMAT: &str = "--format=%(refname)%09%(objectname)%09%(committerdate:unix)%09%(upstream:short)%09%(upstream:track,nobracket)%09%(symref)";
    const FAST_FORMAT: &str = "--format=%(refname)%09%(objectname)%09%(committerdate:unix)%09%(upstream:short)%09%09%(symref)";
    let format = if exact_sync { FULL_FORMAT } else { FAST_FORMAT };
    let output = git::git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        format,
        "refs/heads",
        "refs/remotes",
    ]);
    let mut snapshot = RefSnapshot::default();
    for line in output.lines() {
        let mut fields = line.splitn(6, '\t');
        let refname = fields.next().unwrap_or_default();
        let head = fields.next().unwrap_or_default();
        let when_ts = fields.next().unwrap_or_default().parse().unwrap_or(0);
        let upstream = fields.next().unwrap_or_default();
        let tracking = fields.next().unwrap_or_default();
        let symref = fields.next().unwrap_or_default();

        if let Some(branch) = refname.strip_prefix("refs/heads/") {
            snapshot.local.insert(
                branch.to_string(),
                RefMetadata {
                    head: head.to_string(),
                    when_ts,
                    upstream: upstream.to_string(),
                    tracking: tracking.to_string(),
                },
            );
        } else if let Some(remote) = refname.strip_prefix("refs/remotes/") {
            snapshot
                .remote_heads
                .insert(refname.to_string(), head.to_string());
            if refname == "refs/remotes/origin/HEAD" && !symref.is_empty() {
                snapshot.origin_head = Some(symref.to_string());
            }
            if symref.is_empty() {
                if let Some(branch) = origin_local_branch(remote) {
                    snapshot.origin.insert(
                        branch.to_string(),
                        RefMetadata {
                            head: head.to_string(),
                            when_ts,
                            upstream: String::new(),
                            tracking: String::new(),
                        },
                    );
                }
            }
        }
    }
    snapshot
}

/// Convert an `origin/<branch>` remote-tracking name to its local branch name.
/// The symbolic remote HEAD and option-like local names are never checkout candidates.
pub fn origin_local_branch(remote: &str) -> Option<&str> {
    let branch = remote.strip_prefix("origin/")?;
    (!branch.is_empty() && branch != "HEAD" && !branch.starts_with('-')).then_some(branch)
}

fn resolve_base_ref(config: &Config, refs: &RefSnapshot, current_branch: &str) -> String {
    if let Some(configured) = config
        .base_branch
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        if refs
            .remote_heads
            .contains_key(&format!("refs/remotes/origin/{configured}"))
        {
            return format!("origin/{configured}");
        }
        if refs.local.contains_key(configured) {
            return configured.to_string();
        }
    }

    if let Some(remote) = refs.origin_head.as_deref() {
        if refs.remote_heads.contains_key(remote) {
            return remote
                .strip_prefix("refs/remotes/")
                .unwrap_or(remote)
                .to_string();
        }
        if let Some(local) = remote.strip_prefix("refs/remotes/origin/") {
            if refs.local.contains_key(local) {
                return local.to_string();
            }
        }
    }

    for candidate in ["main", "master"] {
        if refs
            .remote_heads
            .contains_key(&format!("refs/remotes/origin/{candidate}"))
        {
            return format!("origin/{candidate}");
        }
    }
    for candidate in ["main", "master"] {
        if refs.local.contains_key(candidate) {
            return candidate.to_string();
        }
    }
    current_branch.to_string()
}

#[allow(clippy::too_many_arguments)]
fn compute_one(
    wt: &RawWorktree,
    checked_out: bool,
    inspect_worktree: bool,
    repo: &str,
    base_short: &str,
    prefix: &str,
    metadata: Option<&RefMetadata>,
    remote_heads: &HashMap<String, String>,
    exact_sync: bool,
) -> Worktree {
    let created_ts = if checked_out {
        worktree_created_ts(&wt.path)
    } else {
        0.0
    };
    let when_ts = metadata
        .map(|metadata| metadata.when_ts)
        .unwrap_or_else(|| {
            git::git_stdout(&["-C", repo, "log", "-1", "--format=%ct", &wt.head])
                .trim()
                .parse()
                .unwrap_or(0)
        });
    let when = render::relative_age(when_ts);
    let (staged, unstaged, changes, dirty) = if !checked_out {
        (0, 0, "—".to_string(), false)
    } else if !inspect_worktree {
        (0, 0, "…".to_string(), false)
    } else {
        let (staged, unstaged) = git_status_counts(&wt.path, false);
        let changes = if staged == 0 && unstaged == 0 {
            "clean".to_string()
        } else {
            format!("+{staged} ~{unstaged}")
        };
        (staged, unstaged, changes, staged > 0 || unstaged > 0)
    };
    let is_main = checked_out && Path::new(&wt.path).join(".git").is_dir();
    let branch_short = branch_short_name(&wt.branch, prefix);
    let detached = wt.branch.is_empty();
    let is_base = !detached && wt.branch == base_short;

    let (sync, sync_kind) = if detached {
        ("—".to_string(), "detached".to_string())
    } else {
        let (kind, text) = compute_sync(wt, metadata, remote_heads, repo, exact_sync);
        (text, kind)
    };

    Worktree {
        path: if checked_out {
            wt.path.clone()
        } else {
            String::new()
        },
        branch: wt.branch.clone(),
        branch_short,
        head: wt.head.clone(),
        detached,
        is_main,
        is_base,
        when,
        when_ts,
        staged,
        unstaged,
        changes,
        dirty,
        sync,
        sync_kind,
        pr_number: None,
        pr_url: None,
        review: "—".to_string(),
        conflict: false,
        created_ts,
    }
}

fn compute_remote_candidate(
    branch: &str,
    metadata: &RefMetadata,
    base_short: &str,
    prefix: &str,
) -> Worktree {
    let remote_branch = format!("origin/{branch}");
    Worktree {
        path: String::new(),
        branch: remote_branch,
        branch_short: branch_short_name(branch, prefix),
        head: metadata.head.clone(),
        detached: false,
        is_main: false,
        is_base: branch == base_short,
        when: render::relative_age(metadata.when_ts),
        when_ts: metadata.when_ts,
        staged: 0,
        unstaged: 0,
        changes: "—".to_string(),
        dirty: false,
        sync: "remote".to_string(),
        sync_kind: "remote".to_string(),
        pr_number: None,
        pr_url: None,
        review: "—".to_string(),
        conflict: false,
        created_ts: 0.0,
    }
}

fn compute_sync(
    wt: &RawWorktree,
    metadata: Option<&RefMetadata>,
    remote_heads: &HashMap<String, String>,
    repo: &str,
    exact_sync: bool,
) -> (String, String) {
    let Some(metadata) = metadata else {
        let upstream = git::branch_upstream(repo, &wt.branch);
        return status::compute_sync(&wt.branch, upstream.as_deref(), repo);
    };

    if !metadata.upstream.is_empty() {
        if !exact_sync {
            return ("loading".to_string(), "…".to_string());
        }
        return status::sync_from_tracking(&metadata.tracking)
            .unwrap_or_else(|| status::compute_sync(&wt.branch, Some(&metadata.upstream), repo));
    }

    let remote_ref = format!("refs/remotes/origin/{}", wt.branch);
    let Some(remote_head) = remote_heads.get(&remote_ref) else {
        return status::local();
    };
    if remote_head == &wt.head {
        return status::sync_from_tracking("").expect("empty tracking state is valid");
    }
    if !exact_sync {
        return ("loading".to_string(), "…".to_string());
    }
    status::compute_sync_existing(&wt.branch, &format!("origin/{}", wt.branch), repo)
}

fn apply_pr(worktree: &mut Worktree, pr: Option<&PrInfo>) {
    worktree.pr_number = pr.and_then(|pr| pr.number);
    worktree.pr_url = pr.and_then(|pr| pr.url.clone());
    worktree.review = review_display(pr);
    worktree.conflict = pr.map(|pr| pr.conflict).unwrap_or(false);
    if pr.is_some_and(|pr| pr.merged && pr.head_oid.as_deref() == Some(worktree.head.as_str())) {
        worktree.sync = "merged".to_string();
        worktree.sync_kind = "merged".to_string();
    }
}

/// Map a PR to the compact review label.
fn review_display(pr: Option<&PrInfo>) -> String {
    match pr {
        None => "—".to_string(),
        Some(p) if !p.has_pr() || p.merged => "—".to_string(),
        Some(p) if p.is_draft => "draft".to_string(),
        Some(p) => match p.review.as_deref() {
            Some("APPROVED") => "approved".to_string(),
            Some("CHANGES_REQUESTED") => "changes".to_string(),
            _ => "review".to_string(),
        },
    }
}

/// Checkout time: a linked worktree's `.git` pointer file is written once at
/// `git worktree add` and never touched again, so its mtime is the creation
/// time. The main checkout returns 0 so it sorts last.
fn worktree_created_ts(path: &str) -> f64 {
    let git_file = Path::new(path).join(".git");
    if !git_file.is_file() {
        return 0.0;
    }
    match std::fs::metadata(&git_file).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

fn git_status_counts(path: &str, include_untracked: bool) -> (u32, u32) {
    // Picker scans run concurrently in the background; do not let them rewrite
    // indexes or contend on optional locks merely to cache stat information.
    let mut args = vec!["--no-optional-locks", "-C", path, "status", "--porcelain"];
    if !include_untracked {
        // Skipping the untracked-file walk is ~3x faster; huge trees are
        // dominated by enumerating untracked files even when there are none.
        args.push("--untracked-files=no");
    }
    status_counts(&git::git_stdout(&args))
}

fn status_counts(out: &str) -> (u32, u32) {
    let mut staged = 0u32;
    let mut unstaged = 0u32;
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        let x = bytes[0] as char;
        let y = if bytes.len() > 1 {
            bytes[1] as char
        } else {
            ' '
        };
        if matches!(x, 'M' | 'A' | 'D' | 'R' | 'C') {
            staged += 1;
        }
        if matches!(y, 'M' | 'D') {
            unstaged += 1;
        }
        if x == '?' && y == '?' {
            unstaged += 1;
        }
    }
    (staged, unstaged)
}

/// True if the worktree has any uncommitted changes (tracked or untracked).
/// Used at delete time for a precise dirty check; the picker list itself uses
/// the fast tracked-only status above.
pub fn worktree_dirty(path: &str) -> bool {
    worktree_dirty_checked(path).unwrap_or(true)
}

/// Checked variant for destructive actions. Git failures are errors rather
/// than an empty status that could be mistaken for a clean worktree.
pub fn worktree_dirty_checked(path: &str) -> Result<bool> {
    let output = git::git_output(&[
        "--no-optional-locks",
        "-C",
        path,
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    ])?;
    if !output.status.success() {
        anyhow::bail!("git status failed for {path}");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (staged, unstaged) = status_counts(&text);
    Ok(staged > 0 || unstaged > 0)
}

/// 1-based item index of `path` (field 2) in the fzf list, for pre-selection.
pub fn fzf_line_index(list: &str, path: &str) -> Option<usize> {
    let items: Box<dyn Iterator<Item = &str>> = if list.contains('\0') {
        Box::new(list.split_terminator('\0'))
    } else {
        Box::new(list.lines())
    };
    for (i, line) in items.enumerate() {
        let mut it = line.split('\t');
        it.next();
        if it.next() == Some(path) {
            return Some(i + 1);
        }
    }
    None
}

/// Render the picker list (6 tab-separated fields per line).
///
/// Fields are branch, path, entry kind, sync kind, changes, and display.
pub fn render_fzf_lines(engine: &Engine, skip_detached: bool) -> String {
    let colors = crate::theme::ThemeColors::load();
    render_fzf_lines_with_colors(engine, skip_detached, &colors)
}

pub(crate) fn render_fzf_lines_with_colors(
    engine: &Engine,
    skip_detached: bool,
    colors: &crate::theme::ThemeColors,
) -> String {
    let row_count = engine.worktrees.len() + engine.branches.len() + engine.remote_branches.len();
    let mut out = String::with_capacity(row_count * 192);
    for wt in &engine.worktrees {
        if wt.detached && skip_detached {
            continue;
        }
        push_fzf_row(&mut out, wt, "worktree", &engine.prefix, &colors.worktrees);
    }
    for branch in &engine.branches {
        push_fzf_row(&mut out, branch, "branch", &engine.prefix, &colors.branches);
    }
    let remote_prefix = (!engine.prefix.is_empty()).then(|| format!("origin/{}", engine.prefix));
    for branch in &engine.remote_branches {
        push_fzf_row(
            &mut out,
            branch,
            "remote",
            remote_prefix.as_deref().unwrap_or_default(),
            &colors.branches,
        );
    }
    out
}

fn push_fzf_row(
    out: &mut String,
    wt: &Worktree,
    entry_kind: &str,
    prefix: &str,
    color: &crate::theme::AnsiColor,
) {
    let branch_disp = if wt.branch.is_empty() {
        "(detached)"
    } else {
        &wt.branch
    };
    let row = render::render_picker_row(branch_disp, wt, prefix, color);
    writeln!(
        out,
        "{branch_disp}\t{}\t{entry_kind}\t{}\t{}\t{row}",
        wt.path, wt.sync_kind, wt.changes
    )
    .expect("writing to a String cannot fail");
}

/// The `--json` / `--fzf` / `--header` data-engine entry point.
pub fn run_engine(args: &[String]) -> Result<()> {
    let mut format = "json";
    let mut use_cache = true;
    let mut skip_detached = false;
    let mut fast = false;
    for a in args {
        match a.as_str() {
            "--json" => format = "json",
            "--fzf" => format = "fzf",
            "--header" => format = "header",
            "--no-cache" => use_cache = false,
            "--no-detached" => skip_detached = true,
            "--fast" => fast = true,
            _ => {}
        }
    }

    if format == "header" {
        println!("{}", render::render_header());
        return Ok(());
    }

    let repo = git::repo_root()?.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = state_dir();
    let engine = if fast {
        compute_picker_initial(&repo, &config, &state_dir)
    } else {
        compute_all(&repo, &config, &state_dir, use_cache)
    };

    match format {
        "json" => println!("{}", serde_json::to_string(&engine)?),
        "fzf" => print!("{}", render_fzf_lines(&engine, skip_detached)),
        _ => {}
    }
    Ok(())
}
