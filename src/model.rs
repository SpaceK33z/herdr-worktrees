//! Worktree metadata engine: discover worktrees, compute each one in parallel,
//! sort newest-first, and render `--json` / `--fzf` / `--header` output.

use crate::config::{branch_short_name, Config};
use crate::git;
use crate::pr::{self, PrInfo};
use crate::render;
use crate::status;
use crate::util;
use anyhow::Result;
use serde::Serialize;
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
    pub status: String,
    pub status_kind: String,
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
pub fn compute_all(repo: &str, config: &Config, state_dir: &Path, use_cache: bool) -> Engine {
    let porcelain = git::git_stdout(&["-C", repo, "worktree", "list", "--porcelain"]);
    let records = parse_worktree_list(&porcelain);

    let current_branch = git::current_branch();
    let base = git::resolve_base_ref(config, repo, &current_branch);
    let base_short = util::strip_remote(&base);
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let repo_name = Path::new(repo)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fetch_prs = config.github_prs() && git::has_github_remote(repo);

    let checked_out: std::collections::HashSet<&str> = records
        .iter()
        .filter(|record| !record.branch.is_empty())
        .map(|record| record.branch.as_str())
        .collect();
    let branch_records: Vec<RawWorktree> = git::git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(refname:short)\t%(objectname)",
        "refs/heads",
    ])
    .lines()
    .filter_map(|line| {
        let (branch, head) = line.split_once('\t')?;
        if branch.is_empty() || checked_out.contains(branch) {
            return None;
        }
        Some(RawWorktree {
            path: repo.to_string(),
            head: head.to_string(),
            branch: branch.to_string(),
        })
    })
    .collect();

    let (worktrees, branches): (Vec<Worktree>, Vec<Worktree>) = std::thread::scope(|scope| {
        let worktree_handles: Vec<_> = records
            .iter()
            .map(|rec| {
                scope.spawn(|| {
                    compute_one(
                        rec,
                        true,
                        repo,
                        &base,
                        &base_short,
                        &prefix,
                        use_cache,
                        state_dir,
                        fetch_prs,
                    )
                })
            })
            .collect();
        let branch_handles: Vec<_> = branch_records
            .iter()
            .map(|rec| {
                scope.spawn(|| {
                    compute_one(
                        rec,
                        false,
                        repo,
                        &base,
                        &base_short,
                        &prefix,
                        use_cache,
                        state_dir,
                        fetch_prs,
                    )
                })
            })
            .collect();
        (
            worktree_handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect(),
            branch_handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect(),
        )
    });

    let mut worktrees = worktrees;
    worktrees.sort_by(|a, b| {
        b.created_ts
            .partial_cmp(&a.created_ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut branches = branches;
    branches.sort_by(|a, b| b.when_ts.cmp(&a.when_ts));

    Engine {
        repo_path: repo.to_string(),
        repo_name,
        base,
        base_short,
        prefix,
        worktrees,
        branches,
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_one(
    wt: &RawWorktree,
    checked_out: bool,
    repo: &str,
    base: &str,
    base_short: &str,
    prefix: &str,
    use_cache: bool,
    state_dir: &Path,
    fetch_prs: bool,
) -> Worktree {
    let created_ts = if checked_out {
        worktree_created_ts(&wt.path)
    } else {
        0.0
    };
    let revision = if wt.branch.is_empty() {
        wt.head.as_str()
    } else {
        wt.branch.as_str()
    };
    let when_ts: i64 = git::git_stdout(&[
        "-C",
        repo,
        "log",
        "-1",
        "--format=%ct",
        revision,
    ])
    .trim()
    .parse()
    .unwrap_or(0);
    let when = render::relative_age(when_ts);
    let (staged, unstaged) = if checked_out {
        git_status_counts(&wt.path, false)
    } else {
        (0, 0)
    };
    let changes = if !checked_out {
        "—".to_string()
    } else if staged == 0 && unstaged == 0 {
        "clean".to_string()
    } else {
        format!("+{staged} ~{unstaged}")
    };
    let dirty = checked_out && (staged > 0 || unstaged > 0);
    let is_main = checked_out && Path::new(&wt.path).join(".git").is_dir();
    let branch_short = branch_short_name(&wt.branch, prefix);
    let detached = wt.branch.is_empty();
    let is_base = !detached && wt.branch == base_short;

    let pr = if fetch_prs && !detached && !is_base {
        pr::fetch(&wt.branch, repo, state_dir)
    } else {
        None
    };
    let pr_status = match pr.as_ref() {
        Some(p) if p.merged => status::PrStatus::Merged,
        Some(p) if p.has_pr() => status::PrStatus::Unmerged,
        _ => status::PrStatus::None,
    };

    let (status, status_kind) = if detached {
        ("—".to_string(), "detached".to_string())
    } else if is_base {
        ("—".to_string(), "base".to_string())
    } else {
        let (k, t) = status::compute_status(
            &wt.branch,
            base,
            &wt.head,
            use_cache,
            repo,
            state_dir,
            pr_status,
        );
        let k = if k.is_empty() { "other".to_string() } else { k };
        let t = if t.is_empty() { "—".to_string() } else { t };
        (t, k)
    };

    let review = review_display(pr.as_ref());

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
        status,
        status_kind,
        pr_number: pr.as_ref().and_then(|p| p.number),
        pr_url: pr.as_ref().and_then(|p| p.url.clone()),
        review,
        conflict: pr.as_ref().map(|p| p.conflict).unwrap_or(false),
        created_ts,
    }
}

/// Map a PR to the compact review label.
fn review_display(pr: Option<&PrInfo>) -> String {
    match pr {
        None => "—".to_string(),
        Some(p) if p.merged => "—".to_string(),
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
    let mut args = vec!["-C", path, "status", "--porcelain"];
    if !include_untracked {
        // Skipping the untracked-file walk is ~3x faster; huge trees are
        // dominated by enumerating untracked files even when there are none.
        args.push("--untracked-files=no");
    }
    let out = git::git_stdout(&args);
    let mut staged = 0u32;
    let mut unstaged = 0u32;
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        let x = bytes[0] as char;
        let y = if bytes.len() > 1 { bytes[1] as char } else { ' ' };
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
    let (s, u) = git_status_counts(path, true);
    s > 0 || u > 0
}

/// 1-based line index of `path` (field 2) in the fzf list, for pre-selection.
pub fn fzf_line_index(list: &str, path: &str) -> Option<usize> {
    for (i, line) in list.lines().enumerate() {
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
/// Fields are branch, path, entry kind, status kind, changes, and display.
/// Section rows make the two groups visually distinct; callers ignore them.
pub fn render_fzf_lines(engine: &Engine, skip_detached: bool) -> String {
    let mut out = String::new();
    if engine
        .worktrees
        .iter()
        .any(|wt| !wt.detached || !skip_detached)
    {
        out.push_str("\t\tsection\t\t\t\x1b[1mWORKTREES\x1b[0m\n");
        for wt in &engine.worktrees {
            if wt.detached && skip_detached {
                continue;
            }
            push_fzf_row(&mut out, wt, "worktree", &engine.prefix);
        }
    }
    if !engine.branches.is_empty() {
        out.push_str("\t\tsection\t\t\t\x1b[1mBRANCHES\x1b[0m\n");
        for branch in &engine.branches {
            push_fzf_row(&mut out, branch, "branch", &engine.prefix);
        }
    }
    out
}

fn push_fzf_row(out: &mut String, wt: &Worktree, entry_kind: &str, prefix: &str) {
    let branch_disp = if wt.branch.is_empty() {
        "(detached)".to_string()
    } else {
        wt.branch.clone()
    };
    let row = render::render_row(&branch_disp, wt, prefix);
    out.push_str(&format!(
        "{branch_disp}\t{}\t{entry_kind}\t{}\t{}\t{row}\n",
        wt.path, wt.status_kind, wt.changes
    ));
}

/// The `--json` / `--fzf` / `--header` data-engine entry point.
pub fn run_engine(args: &[String]) -> Result<()> {
    let mut format = "json";
    let mut use_cache = true;
    let mut skip_detached = false;
    for a in args {
        match a.as_str() {
            "--json" => format = "json",
            "--fzf" => format = "fzf",
            "--header" => format = "header",
            "--no-cache" => use_cache = false,
            "--no-detached" => skip_detached = true,
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
    let engine = compute_all(&repo, &config, &state_dir, use_cache);

    match format {
        "json" => println!("{}", serde_json::to_string(&engine)?),
        "fzf" => print!("{}", render_fzf_lines(&engine, skip_detached)),
        _ => {}
    }
    Ok(())
}
