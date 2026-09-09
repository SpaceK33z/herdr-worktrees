//! Fetch GitHub PR data for a branch (`gh pr list`), with a short-TTL cache.

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: Option<u32>,
    pub url: Option<String>,
    pub is_draft: bool,
    pub review: Option<String>,
    pub merged: bool,
    pub conflict: bool,
    #[serde(default)]
    pub head_oid: Option<String>,
    #[serde(default)]
    pub unresolved_threads: Option<u32>,
    #[serde(default)]
    pub threads_truncated: bool,
    #[serde(skip)]
    node_id: Option<String>,
}

impl PrInfo {
    pub fn has_pr(&self) -> bool {
        self.number.is_some()
    }
}

/// A single pull request resolved by number for an explicit checkout. This is
/// separate from the background `PrInfo` cache: it is fetched synchronously on
/// Enter, not while the user types.
#[derive(Debug, Clone)]
pub struct PullRequestTarget {
    pub number: u32,
    pub head_ref: String,
    pub head_oid: String,
    pub base_ref: String,
    pub is_cross_repo: bool,
    pub head_repository: String,
}

/// Timeouts per `gh` invocation. The explicit PR lookup is a user action and
/// may talk to a slow network; the picker's background queries run while the
/// user types and must not stall the list.
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(5000);
const LIST_TIMEOUT: Duration = Duration::from_millis(1500);
const INTERACTIVE_LIST_TIMEOUT: Duration = Duration::from_millis(5000);
const THREADS_TIMEOUT: Duration = Duration::from_millis(2000);

/// How long the branch listing may run. The picker's background refresh draws
/// the list and must not stall it; a deliberate refresh (`ctrl-r`, `ctrl-f`) is
/// a user action that can afford a slow network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPatience {
    Background,
    Interactive,
}

impl FetchPatience {
    fn list_timeout(self) -> Duration {
        match self {
            FetchPatience::Background => LIST_TIMEOUT,
            FetchPatience::Interactive => INTERACTIVE_LIST_TIMEOUT,
        }
    }
}

/// Why a `gh` invocation produced no usable output. Kept distinguishable so a
/// synchronous user action can say what actually went wrong.
#[derive(Debug, PartialEq, Eq)]
enum GhError {
    /// `gh` could not be started — usually not installed or not on `PATH`.
    NotInstalled,
    /// Still running when the timeout expired; the child was killed.
    TimedOut(Duration),
    /// Ran but failed, carrying the first line of stderr when it was captured.
    Failed(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GhError::NotInstalled => write!(f, "gh is not installed or not on PATH"),
            GhError::TimedOut(after) => {
                write!(f, "gh timed out after {:.0}s", after.as_secs_f32())
            }
            GhError::Failed(detail) if detail.is_empty() => write!(f, "gh exited with an error"),
            GhError::Failed(detail) => write!(f, "gh: {detail}"),
        }
    }
}

/// Run `gh` in `repo` under a hard timeout and return its stdout. `gh` writes
/// its explanation to stderr, so capture it wherever the failure is shown.
fn gh_output(
    repo: &str,
    args: &[&str],
    timeout: Duration,
    capture_stderr: bool,
) -> Result<String, GhError> {
    let stderr = if capture_stderr {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    };
    let mut command = std::process::Command::new("gh");
    // Do not let gh's separately configured default remote change PR identity.
    let ctx = crate::detect::RepoCtx::new(repo);
    if !ctx.host.is_empty() && !ctx.owner.is_empty() && !ctx.name.is_empty() {
        command.env(
            "GH_REPO",
            format!("{}/{}/{}", ctx.host, ctx.owner, ctx.name),
        );
    }
    command
        .args(args)
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(stderr);
    run_with_timeout(command, timeout)
}

/// Run a prepared command and return its stdout, killing it if it outlives
/// `timeout`.
///
/// The `Child` stays owned here and is killed through it, so the kernel cannot
/// recycle its pid onto an unrelated process before the signal lands — which
/// signalling a copied raw pid does not guarantee. Both pipes are drained on
/// their own threads, so a child that outruns the pipe buffer cannot deadlock
/// against the wait, and the waiter reaps the child whether it exits on its own
/// or is killed.
fn run_with_timeout(
    mut command: std::process::Command,
    timeout: Duration,
) -> Result<String, GhError> {
    let mut child = command.spawn().map_err(|_| GhError::NotInstalled)?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let waiter = Arc::clone(&child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let errors = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let mut out = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut out);
        }
        let err = errors.join().unwrap_or_default();
        // Both pipes are at EOF here, so the child has exited or is about to:
        // the lock is held only for a wait that returns immediately.
        let status = waiter.lock().map(|mut child| child.wait());
        let _ = tx.send((status.ok(), out, err));
    });

    match rx.recv_timeout(timeout) {
        Ok((Some(Ok(status)), out, err)) => {
            if status.success() {
                Ok(String::from_utf8_lossy(&out).into_owned())
            } else {
                Err(GhError::Failed(first_line(&err)))
            }
        }
        Ok(_) => Err(GhError::Failed(String::new())),
        Err(_) => {
            // `try_lock` keeps this non-blocking: a held lock means the waiter
            // is already inside `wait`, so the child is finishing on its own.
            if let Ok(mut child) = child.try_lock() {
                let _ = child.kill();
            }
            Err(GhError::TimedOut(timeout))
        }
    }
}

/// First non-blank line of captured stderr — `gh` puts its explanation there.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Resolve a pull request by number, reporting why it failed. An explicit user
/// action, so it gets a longer timeout than the background picker cache and
/// captures stderr for diagnostics.
pub fn resolve_pr_detailed(number: u32, repo: &str) -> Result<PullRequestTarget, String> {
    let fields = "number,headRefName,headRefOid,baseRefName,isCrossRepository,headRepository";
    let number = number.to_string();
    let json = gh_output(
        repo,
        &["pr", "view", &number, "--json", fields],
        RESOLVE_TIMEOUT,
        true,
    )
    .map_err(|err| err.to_string())?;
    parse_pr_target(&json).ok_or_else(|| "gh returned an unexpected response".to_string())
}

fn parse_pr_target(json: &str) -> Option<PullRequestTarget> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(PullRequestTarget {
        number: value.get("number")?.as_u64()? as u32,
        head_ref: value.get("headRefName")?.as_str()?.to_string(),
        head_oid: value.get("headRefOid")?.as_str()?.to_string(),
        base_ref: value.get("baseRefName")?.as_str()?.to_string(),
        is_cross_repo: value.get("isCrossRepository")?.as_bool()?,
        head_repository: value
            .get("headRepository")?
            .get("nameWithOwner")?
            .as_str()?
            .to_string(),
    })
}

const CACHE_TTL_MS: u128 = 60_000;
/// The two repository-wide listings every refresh runs: open pull requests
/// (what is still actionable) and the most recently merged ones (what makes a
/// worktree safe to delete). `mergeStateStatus` is asked for only where it is
/// used — a conflicting open pull request — because GitHub computes mergeability
/// per row and it roughly doubles the time a listing takes.
const LISTINGS: [(&str, &str); 2] = [
    (
        "open",
        "id,number,url,state,isDraft,reviewDecision,mergeStateStatus,headRefName,headRefOid,isCrossRepository,headRepository",
    ),
    (
        "merged",
        "id,number,url,state,isDraft,reviewDecision,headRefName,headRefOid,isCrossRepository,headRepository",
    ),
];
/// One page per listing. Paging is what costs time here: a busy repository
/// answers a single page in about a second and its whole pull request history
/// in half a minute — long past any timeout a picker refresh can wait out, which
/// is why the listings stay shallow and the per-branch queries below fill in
/// whatever they missed.
const LISTING_LIMIT: usize = 100;
/// Fields for a per-branch query. Only a handful of rows come back, so the
/// expensive `mergeStateStatus` is affordable here.
const BRANCH_FIELDS: &str =
    "id,number,url,state,isDraft,reviewDecision,mergeStateStatus,headRefName,headRefOid,isCrossRepository,headRepository";
/// Pull requests fetched per branch. Only the newest open one, or a merged one
/// that still covers the branch head, is ever shown; a handful of rows is more
/// than enough history to choose from.
const BRANCH_PR_LIMIT: usize = 10;
/// How many branches may get their own query in one refresh. Each costs a `gh`
/// process, and a long-lived checkout can have hundreds of stale branches, so
/// the budget goes to the branches the caller listed first — the checked-out
/// ones the picker is really about.
const TARGETED_BRANCHES: usize = 32;
/// How many per-branch queries run at once.
const PR_WORKERS: usize = 8;
const THREAD_LIMIT: usize = 100;
/// Cache files untouched for this long are deleted on the next refresh.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const REFRESH_FILE: &str = "last-refresh.json";

/// A branch and the object id its local ref points at.
type BranchHead = (String, String);

/// Fetch the pull request for each requested branch. Branches already in the
/// cache avoid network requests; local Git config still validates repository
/// provenance so rebinding a branch cannot reuse another repository's PR.
///
/// Two shallow repository-wide listings answer most branches in one round trip
/// each; whatever they miss — an older merged pull request, typically, which is
/// exactly what says a worktree is finished with — is asked for one branch at a
/// time. Listing the repository's entire pull request history instead would
/// answer everything in one query, but on a busy repository it cannot be paged
/// through inside any timeout a refresh can wait out, and the merged branches
/// are the ones that then never appear.
pub fn fetch_many(
    branch_heads: &[BranchHead],
    repo: &str,
    state_dir: &Path,
    use_cache: bool,
    patience: FetchPatience,
) -> HashMap<String, PrInfo> {
    let mut cached = HashMap::with_capacity(branch_heads.len());
    let mut wanted: Vec<&BranchHead> = Vec::new();
    for entry in branch_heads {
        // A cached entry is read even when the caller asked for fresh data: it
        // is what the row falls back to if this branch's query fails.
        if let Some(info) = cache_get(state_dir, repo, &entry.0) {
            cached.insert(entry.0.clone(), info);
            if use_cache {
                continue;
            }
        }
        wanted.push(entry);
    }
    if wanted.is_empty() {
        return cached;
    }

    let (rows, mut failed) = fetch_listings(repo, patience);
    let mut fresh = resolve_from_listings(&rows, &wanted, repo);

    let unresolved: Vec<&BranchHead> = wanted
        .iter()
        .copied()
        .filter(|(branch, _)| !fresh.contains_key(branch))
        .take(TARGETED_BRANCHES)
        .collect();
    let targeted = util::parallel_map(&unresolved, PR_WORKERS, |(branch, head)| {
        fetch_branch(repo, branch, head, patience)
    });
    for ((branch, _), info) in unresolved.iter().zip(targeted) {
        match info.flatten() {
            Some(info) => {
                fresh.insert(branch.clone(), info);
            }
            None => failed = true,
        }
    }

    if fresh.is_empty() {
        // Empty PR columns otherwise look like "this repository has no pull
        // requests"; record the failure so the picker footer can say so.
        if failed {
            store_refresh_failure(state_dir, repo);
        }
        return cached;
    }

    let node_ids: Vec<_> = fresh
        .values()
        .filter(|info| info.has_pr() && !info.merged)
        .filter_map(|info| info.node_id.clone())
        .collect();
    if let Some(thread_counts) = gh_fetch_thread_counts(repo, &node_ids) {
        for info in fresh.values_mut() {
            if let Some(count) = info.node_id.as_deref().and_then(|id| thread_counts.get(id)) {
                info.unresolved_threads = Some(count.unresolved);
                info.threads_truncated = count.truncated;
            }
        }
    }
    if failed {
        store_refresh_failure(state_dir, repo);
    } else {
        store_last_refresh(state_dir, repo);
        prune_stale_cache(state_dir, repo);
    }
    for (branch, info) in fresh {
        cache_store(state_dir, repo, &branch, &info);
        cached.insert(branch, info);
    }
    cached
}

/// Run both repository-wide listings at once and return their rows, plus
/// whether either one failed to answer.
fn fetch_listings(repo: &str, patience: FetchPatience) -> (Vec<serde_json::Value>, bool) {
    let mut rows = Vec::new();
    let mut failed = false;
    let listed = util::parallel_map(&LISTINGS, LISTINGS.len(), |(state, fields)| {
        gh_fetch_listing(repo, state, fields, patience)
    });
    for listing in listed {
        match listing.flatten() {
            Some(mut listing) => rows.append(&mut listing),
            None => failed = true,
        }
    }
    (rows, failed)
}

/// Match listed pull requests to the branches we asked about. A branch with no
/// row stays unresolved rather than being recorded as having none: the listings
/// are capped, so absence from them proves nothing.
fn resolve_from_listings(
    rows: &[serde_json::Value],
    wanted: &[&BranchHead],
    repo: &str,
) -> HashMap<String, PrInfo> {
    let mut by_branch: HashMap<&str, Vec<&serde_json::Value>> = HashMap::new();
    for row in rows {
        if let Some(branch) = row.get("headRefName").and_then(|name| name.as_str()) {
            by_branch.entry(branch).or_default().push(row);
        }
    }
    wanted
        .iter()
        .filter_map(|(branch, head)| {
            let candidates = by_branch.get(branch.as_str())?;
            Some((
                branch.clone(),
                select_pr_candidate(candidates, head, fork_repository(repo, branch).as_deref())?,
            ))
        })
        .collect()
}

/// One branch's pull request, or `None` when `gh` could not answer. "This
/// branch has no pull request" is an answer, not a failure: it comes back as a
/// default `PrInfo` so the cache stops asking again.
fn fetch_branch(repo: &str, branch: &str, head: &str, patience: FetchPatience) -> Option<PrInfo> {
    parse_branch_prs_for(
        &gh_fetch_branch(repo, branch, patience)?,
        head,
        fork_repository(repo, branch).as_deref(),
    )
}

#[cfg(test)]
fn parse_branch_prs(json: &str, head: &str) -> Option<PrInfo> {
    parse_branch_prs_for(json, head, None)
}

fn parse_branch_prs_for(json: &str, head: &str, repository: Option<&str>) -> Option<PrInfo> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let prs: Vec<_> = value.as_array()?.iter().collect();
    Some(select_pr_candidate(&prs, head, repository).unwrap_or_default())
}

/// Every pull request ever opened from `branch`, newest first.
fn gh_fetch_branch(repo: &str, branch: &str, patience: FetchPatience) -> Option<String> {
    let limit = BRANCH_PR_LIMIT.to_string();
    gh_run(
        repo,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--head",
            branch,
            "--json",
            BRANCH_FIELDS,
            "--limit",
            &limit,
        ],
        patience,
    )
}

fn gh_fetch_listing(
    repo: &str,
    state: &str,
    fields: &str,
    patience: FetchPatience,
) -> Option<Vec<serde_json::Value>> {
    let limit = LISTING_LIMIT.to_string();
    let json = gh_run(
        repo,
        &[
            "pr", "list", "--state", state, "--json", fields, "--limit", &limit,
        ],
        patience,
    )?;
    match serde_json::from_str(&json).ok()? {
        serde_json::Value::Array(rows) => Some(rows),
        _ => None,
    }
}

/// Run one `gh` query under a hard timeout. A timed-out child is killed and
/// reaped instead of lingering after the picker has moved on.
fn gh_run(repo: &str, args: &[&str], patience: FetchPatience) -> Option<String> {
    gh_output(repo, args, patience.list_timeout(), false).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThreadCount {
    unresolved: u32,
    truncated: bool,
}

fn gh_fetch_thread_counts(repo: &str, node_ids: &[String]) -> Option<HashMap<String, ThreadCount>> {
    if node_ids.is_empty() {
        return Some(HashMap::new());
    }
    let ids = serde_json::to_string(node_ids).ok()?;
    let query = format!(
        "query {{ nodes(ids: {ids}) {{ ... on PullRequest {{ id reviewThreads(first: {THREAD_LIMIT}) {{ nodes {{ isResolved }} pageInfo {{ hasNextPage }} }} }} }} }}"
    );
    let json = gh_output(
        repo,
        &["api", "graphql", "-f", &format!("query={query}")],
        THREADS_TIMEOUT,
        false,
    )
    .ok()?;
    parse_thread_counts(&json)
}

fn parse_thread_counts(json: &str) -> Option<HashMap<String, ThreadCount>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let nodes = value.get("data")?.get("nodes")?.as_array()?;
    let mut counts = HashMap::with_capacity(nodes.len());
    for node in nodes.iter().filter_map(serde_json::Value::as_object) {
        let id = node.get("id")?.as_str()?;
        let threads = node.get("reviewThreads")?;
        let unresolved = threads
            .get("nodes")?
            .as_array()?
            .iter()
            .filter(|thread| {
                thread.get("isResolved").and_then(|value| value.as_bool()) == Some(false)
            })
            .count() as u32;
        let truncated = threads.get("pageInfo")?.get("hasNextPage")?.as_bool()?;
        counts.insert(
            id.to_string(),
            ThreadCount {
                unresolved,
                truncated,
            },
        );
    }
    Some(counts)
}

/// Explicit fork checkout provenance, or the identity of a non-origin
/// tracking remote. An untracked local branch belongs to the origin context;
/// unpublished commits do not change that repository identity.
pub(crate) fn fork_repository(repo: &str, branch: &str) -> Option<String> {
    let config = |key: &str| {
        crate::git::git_stdout(&["-C", repo, "config", "--get", key])
            .trim()
            .to_string()
    };
    let recorded = config(&format!("branch.{branch}.herdr-pr-repository"));
    if !recorded.is_empty() {
        return Some(recorded);
    }
    let remote = config(&format!("branch.{branch}.remote"));
    if remote.is_empty() || remote == "origin" || remote == "." {
        return None;
    }
    let origin = crate::detect::parse_remote(&config("remote.origin.url"));
    let other = crate::detect::parse_remote(&config(&format!("remote.{remote}.url")));
    if other.0.eq_ignore_ascii_case(&origin.0)
        && other.1.eq_ignore_ascii_case(&origin.1)
        && other.2.eq_ignore_ascii_case(&origin.2)
    {
        return None;
    }
    if other.0.eq_ignore_ascii_case(&origin.0) && !other.1.is_empty() && !other.2.is_empty() {
        Some(format!("{}/{}", other.1, other.2))
    } else {
        // Unknown/non-GitHub tracking identity must not receive origin's PR.
        Some(format!("unknown-remote:{remote}"))
    }
}

fn select_pr_candidate(
    prs: &[&serde_json::Value],
    current_head: &str,
    repository: Option<&str>,
) -> Option<PrInfo> {
    let prs: Vec<_> = prs
        .iter()
        .copied()
        .filter(|pr| {
            match (
                pr.get("isCrossRepository").and_then(|v| v.as_bool()),
                repository,
            ) {
                (Some(false), None) => true,
                (Some(true), Some(expected)) => pr
                    .get("headRepository")
                    .and_then(|v| v.get("nameWithOwner"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected)),
                _ => false,
            }
        })
        .collect();
    // An open PR is actionable. Otherwise retain a merged PR only when it
    // covers the branch's current head; later local commits must not inherit a
    // stale `merged` label from an older PR.
    let pr = prs
        .iter()
        .copied()
        .find(|pr| pr.get("state").and_then(|state| state.as_str()) == Some("OPEN"))
        .or_else(|| {
            prs.iter().copied().find(|pr| {
                pr.get("state").and_then(|state| state.as_str()) == Some("MERGED")
                    && pr.get("headRefOid").and_then(|oid| oid.as_str()) == Some(current_head)
            })
        })?;
    let state = pr.get("state").and_then(|s| s.as_str()).unwrap_or("");
    Some(PrInfo {
        number: pr.get("number").and_then(|n| n.as_u64()).map(|n| n as u32),
        url: pr.get("url").and_then(|u| u.as_str()).map(String::from),
        is_draft: pr.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false),
        review: pr
            .get("reviewDecision")
            .and_then(|r| r.as_str())
            .map(String::from),
        merged: state == "MERGED",
        conflict: state == "OPEN"
            && pr.get("mergeStateStatus").and_then(|m| m.as_str()) == Some("DIRTY"),
        head_oid: pr
            .get("headRefOid")
            .and_then(|oid| oid.as_str())
            .map(String::from),
        unresolved_threads: None,
        threads_truncated: false,
        node_id: pr.get("id").and_then(|id| id.as_str()).map(String::from),
    })
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u128,
    repo: String,
    branch: String,
    info: PrInfo,
}

#[derive(Serialize, Deserialize)]
struct RefreshEntry {
    /// When GitHub data was last fetched successfully.
    ts: u128,
    repo: String,
    /// Set when the most recent attempt failed; cleared by the next success, so
    /// the footer can distinguish "no pull requests" from "could not ask".
    #[serde(default)]
    failed: bool,
}

/// What the picker footer says about GitHub: when the data was last fetched,
/// and whether the most recent attempt failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStatus {
    pub refreshed_at: Option<u128>,
    pub failed: bool,
}

pub fn refresh_status(state_dir: &Path, repo: &str) -> RefreshStatus {
    let Some(entry) = read_refresh(state_dir, repo) else {
        return RefreshStatus::default();
    };
    RefreshStatus {
        // A failure before any success leaves no timestamp to report.
        refreshed_at: (entry.ts > 0).then_some(entry.ts),
        failed: entry.failed,
    }
}

fn read_refresh(state_dir: &Path, repo: &str) -> Option<RefreshEntry> {
    let content = std::fs::read_to_string(refresh_file(state_dir, repo)).ok()?;
    let entry: RefreshEntry = serde_json::from_str(&content).ok()?;
    (entry.repo == repo).then_some(entry)
}

fn store_last_refresh(state_dir: &Path, repo: &str) {
    let entry = RefreshEntry {
        ts: now_ms(),
        repo: repo.to_string(),
        failed: false,
    };
    let _ = util::write_json_atomic(&refresh_file(state_dir, repo), &entry);
}

/// Flag the failure while keeping the last successful timestamp, so the footer
/// can still say how old the data on screen is.
fn store_refresh_failure(state_dir: &Path, repo: &str) {
    let entry = RefreshEntry {
        ts: read_refresh(state_dir, repo).map_or(0, |entry| entry.ts),
        repo: repo.to_string(),
        failed: true,
    };
    let _ = util::write_json_atomic(&refresh_file(state_dir, repo), &entry);
}

/// Expired cache entries are only ignored, not deleted, so a branch that is
/// never looked at again leaves its file behind forever. Sweep the repository's
/// cache directory on refresh — the one path that already touches it — so a
/// long-lived state directory does not grow without bound.
fn prune_stale_cache(state_dir: &Path, repo: &str) {
    let Ok(entries) = std::fs::read_dir(repo_cache_dir(state_dir, repo)) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if entry.file_name() == REFRESH_FILE {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > CACHE_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Read whatever the cache already holds for `branches`, without starting `gh`.
/// The picker's first frame uses this so reopening within the TTL draws the PR
/// columns immediately instead of waiting for the background refresh.
pub fn cached_many<'a>(
    branches: impl IntoIterator<Item = &'a str>,
    repo: &str,
    state_dir: &Path,
) -> HashMap<String, PrInfo> {
    branches
        .into_iter()
        .filter_map(|branch| Some((branch.to_string(), cache_get(state_dir, repo, branch)?)))
        .collect()
}

fn cache_get(state_dir: &Path, repo: &str, branch: &str) -> Option<PrInfo> {
    let content = std::fs::read_to_string(cache_file(state_dir, repo, branch)).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;
    if entry.repo == repo
        && entry.branch == branch
        && now_ms() < entry.ts.saturating_add(CACHE_TTL_MS)
    {
        Some(entry.info)
    } else {
        None
    }
}

fn cache_store(state_dir: &Path, repo: &str, branch: &str, info: &PrInfo) {
    let entry = CacheEntry {
        ts: now_ms(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        info: info.clone(),
    };
    let _ = util::write_json_atomic(&cache_file(state_dir, repo, branch), &entry);
}

fn cache_file(state_dir: &Path, repo: &str, branch: &str) -> PathBuf {
    let identity = fork_repository(repo, branch).unwrap_or_default();
    repo_cache_dir(state_dir, repo).join(format!(
        "{:016x}.json",
        cache_hash(&format!("{branch}\0{identity}"))
    ))
}

fn refresh_file(state_dir: &Path, repo: &str) -> PathBuf {
    repo_cache_dir(state_dir, repo).join(REFRESH_FILE)
}

fn repo_cache_dir(state_dir: &Path, repo: &str) -> PathBuf {
    state_dir
        .join("pr-info-v3")
        .join(format!("{:016x}", cache_hash(repo)))
}

fn cache_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        cache_file, cache_store, cached_many, first_line, now_ms, parse_branch_prs,
        parse_pr_target, parse_thread_counts, prune_stale_cache, refresh_file, refresh_status,
        repo_cache_dir, resolve_from_listings, run_with_timeout, store_last_refresh,
        store_refresh_failure, CacheEntry, FetchPatience, GhError, PrInfo, ThreadCount,
        CACHE_MAX_AGE, CACHE_TTL_MS, INTERACTIVE_LIST_TIMEOUT, LIST_TIMEOUT,
    };
    use crate::util;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn state_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-pr-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn pr_target_parses_head_base_and_cross_repo_flag() {
        let json = r#"{
            "number": 42,
            "headRefName": "kees/fix",
            "headRefOid": "abc123",
            "baseRefName": "main",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "fork/repo"}
        }"#;
        let target = parse_pr_target(json).unwrap();
        assert_eq!(target.number, 42);
        assert_eq!(target.head_ref, "kees/fix");
        assert_eq!(target.head_oid, "abc123");
        assert_eq!(target.base_ref, "main");
        assert!(target.is_cross_repo);
    }

    #[test]
    fn pr_target_rejects_malformed_json() {
        assert!(parse_pr_target(r#"{"number": "not-a-number"}"#).is_none());
        assert!(parse_pr_target("not json").is_none());
    }

    #[test]
    fn open_pr_takes_precedence_over_merged_history() {
        let json = r#"[
            {"number":1,"isCrossRepository":false,"state":"MERGED","headRefOid":"head","mergeStateStatus":"CLEAN"},
            {"number":2,"isCrossRepository":false,"state":"OPEN","headRefOid":"head","mergeStateStatus":"DIRTY"}
        ]"#;
        let pr = parse_branch_prs(json, "head").unwrap();
        assert_eq!(pr.number, Some(2));
        assert!(!pr.merged);
        assert!(pr.conflict);
    }

    #[test]
    fn merged_pr_only_applies_to_the_current_head() {
        let json = r#"[
            {"number":1,"isCrossRepository":false,"state":"MERGED","headRefOid":"merged-head","mergeStateStatus":"CLEAN"}
        ]"#;
        let merged = parse_branch_prs(json, "merged-head").unwrap();
        assert_eq!(merged.number, Some(1));
        assert!(merged.merged);

        let stale = parse_branch_prs(json, "new-local-head").unwrap();
        assert_eq!(stale.number, None);
        assert!(!stale.merged);
    }

    /// The remove picker exists to retire finished work, so a branch whose only
    /// pull request was merged long ago must still report it. That is what the
    /// per-branch query is for; the capped listings cannot see back that far.
    #[test]
    fn a_branch_query_reports_a_merged_pull_request() {
        let json = r#"[
            {"number":16071,"isCrossRepository":false,"state":"MERGED","headRefName":"kees/fix","headRefOid":"head"}
        ]"#;
        let pr = parse_branch_prs(json, "head").unwrap();
        assert_eq!(pr.number, Some(16071));
        assert!(pr.merged);
    }

    /// An empty per-branch response is an answer — this branch has no pull
    /// request — and gets cached as one. An unreadable response is not.
    #[test]
    fn a_branch_query_distinguishes_no_pull_request_from_no_answer() {
        let none = parse_branch_prs("[]", "head").unwrap();
        assert_eq!(none.number, None);
        assert!(!none.merged);

        assert!(parse_branch_prs("not json", "head").is_none());
        assert!(parse_branch_prs(r#"{"message":"rate limited"}"#, "head").is_none());
    }

    #[test]
    fn listings_are_grouped_by_branch() {
        let rows: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"number":1,"isCrossRepository":false,"state":"OPEN","headRefName":"one","headRefOid":"one-head"},
            {"number":2,"isCrossRepository":false,"state":"MERGED","headRefName":"two","headRefOid":"two-head"},
            {"number":3,"isCrossRepository":false,"state":"OPEN","headRefName":"other","headRefOid":"other-head"}
        ]"#,
        )
        .unwrap();
        let branches = [
            ("one".to_string(), "one-head".to_string()),
            ("two".to_string(), "two-head".to_string()),
            ("none".to_string(), "none-head".to_string()),
        ];
        let wanted: Vec<_> = branches.iter().collect();
        let prs = resolve_from_listings(&rows, &wanted, "");
        assert_eq!(prs["one"].number, Some(1));
        assert_eq!(prs["two"].number, Some(2));
        assert!(prs["two"].merged);
        // Missing from a capped listing means "not seen", never "has none":
        // caching that as an answer is how a merged branch loses its number.
        assert!(!prs.contains_key("none"));
    }

    #[test]
    fn unresolved_review_threads_are_counted_by_pull_request() {
        let json = r#"{
            "data": {
                "nodes": [
                    {
                        "id": "PR_one",
                        "reviewThreads": {
                            "nodes": [
                                {"isResolved": false},
                                {"isResolved": true},
                                {"isResolved": false}
                            ],
                            "pageInfo": {"hasNextPage": false}
                        }
                    },
                    {
                        "id": "PR_two",
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": {"hasNextPage": true}
                        }
                    }
                ]
            }
        }"#;
        let counts = parse_thread_counts(json).unwrap();
        assert_eq!(
            counts["PR_one"],
            ThreadCount {
                unresolved: 2,
                truncated: false
            }
        );
        assert_eq!(
            counts["PR_two"],
            ThreadCount {
                unresolved: 0,
                truncated: true
            }
        );
    }

    fn sh(script: &str, capture_stderr: bool) -> std::process::Command {
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(if capture_stderr {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            });
        command
    }

    #[test]
    fn timed_run_returns_stdout_larger_than_the_pipe_buffer() {
        let output = run_with_timeout(
            sh("yes abcdefgh | head -n 40000", false),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(output.lines().count(), 40_000);
    }

    #[test]
    fn timed_run_reports_the_first_stderr_line_of_a_failure() {
        let error = run_with_timeout(
            sh("echo 'no PullRequest with that number' >&2; exit 1", true),
            Duration::from_secs(30),
        )
        .unwrap_err();
        assert_eq!(
            error,
            GhError::Failed("no PullRequest with that number".to_string())
        );
        assert_eq!(error.to_string(), "gh: no PullRequest with that number");
    }

    #[test]
    fn timed_run_kills_a_child_that_outlives_the_timeout() {
        let started = std::time::Instant::now();
        let timeout = Duration::from_millis(200);
        let error = run_with_timeout(sh("sleep 30", false), timeout).unwrap_err();
        assert_eq!(error, GhError::TimedOut(timeout));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn timed_run_reports_a_missing_binary() {
        let command = std::process::Command::new("herdr-no-such-binary-cbb1f0");
        assert_eq!(
            run_with_timeout(command, Duration::from_secs(30)).unwrap_err(),
            GhError::NotInstalled
        );
    }

    #[test]
    fn stderr_detail_skips_blank_leading_lines() {
        assert_eq!(first_line(b"\n  \n  boom  \nrest\n"), "boom");
        assert_eq!(first_line(b""), "");
    }

    #[test]
    fn timeout_message_names_the_elapsed_budget() {
        assert_eq!(
            GhError::TimedOut(Duration::from_millis(5000)).to_string(),
            "gh timed out after 5s"
        );
    }

    #[test]
    fn pruning_drops_day_old_cache_files_but_keeps_the_refresh_marker() {
        let state = std::env::temp_dir().join(format!(
            "herdr-pr-prune-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = "/repo/one";
        let dir = repo_cache_dir(&state, repo);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - CACHE_MAX_AGE - Duration::from_secs(60);
        let stale = cache_file(&state, repo, "stale");
        let fresh = cache_file(&state, repo, "fresh");
        let marker = refresh_file(&state, repo);
        for file in [&stale, &fresh, &marker] {
            std::fs::write(file, "{}").unwrap();
        }
        for file in [&stale, &marker] {
            std::fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        prune_stale_cache(&state, repo);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(marker.exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn cached_lookup_returns_only_branches_with_a_fresh_entry() {
        let state = state_dir("cached");
        let repo = "/repo/one";
        cache_store(&state, repo, "stored", &PrInfo::default());

        let found = cached_many(["stored", "unknown"], repo, &state);
        assert!(found.contains_key("stored"));
        assert!(!found.contains_key("unknown"));
        // Entries belong to one repository only.
        assert!(cached_many(["stored"], "/repo/two", &state).is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Write a cache entry with a chosen age, which `cache_store` cannot do.
    fn store_aged(state: &Path, repo: &str, branch: &str, age_ms: u128) {
        let entry = CacheEntry {
            ts: now_ms().saturating_sub(age_ms),
            repo: repo.to_string(),
            branch: branch.to_string(),
            info: PrInfo::default(),
        };
        util::write_json_atomic(&cache_file(state, repo, branch), &entry).unwrap();
    }

    /// The TTL is what makes a reopened picker refetch instead of showing PR
    /// columns from an earlier session forever.
    #[test]
    fn a_cache_entry_stops_being_used_once_its_ttl_has_passed() {
        let state = state_dir("ttl");
        let repo = "/repo/one";

        store_aged(&state, repo, "fresh", CACHE_TTL_MS / 2);
        assert!(cached_many(["fresh"], repo, &state).contains_key("fresh"));

        // Exactly one TTL old: time only moves on between writing and reading,
        // so this entry is expired by the time it is looked up.
        store_aged(&state, repo, "expired", CACHE_TTL_MS);
        assert!(cached_many(["expired"], repo, &state).is_empty());

        store_aged(&state, repo, "ancient", CACHE_TTL_MS.saturating_mul(100));
        assert!(cached_many(["ancient"], repo, &state).is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Cache files are named by a hash of the branch, so the entry carries the
    /// branch as well and a file that does not name it is ignored.
    #[test]
    fn a_cache_entry_naming_another_branch_is_not_used() {
        let state = state_dir("branch");
        let repo = "/repo/one";
        let entry = CacheEntry {
            ts: now_ms(),
            repo: repo.to_string(),
            branch: "other".to_string(),
            info: PrInfo::default(),
        };
        util::write_json_atomic(&cache_file(&state, repo, "wanted"), &entry).unwrap();

        assert!(cached_many(["wanted"], repo, &state).is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_failed_refresh_is_recorded_without_losing_the_last_success() {
        let state = state_dir("refresh");
        let repo = "/repo/one";
        assert_eq!(refresh_status(&state, repo).refreshed_at, None);
        assert!(!refresh_status(&state, repo).failed);

        store_last_refresh(&state, repo);
        let after_success = refresh_status(&state, repo);
        assert!(after_success.refreshed_at.is_some());
        assert!(!after_success.failed);

        store_refresh_failure(&state, repo);
        let after_failure = refresh_status(&state, repo);
        assert_eq!(after_failure.refreshed_at, after_success.refreshed_at);
        assert!(after_failure.failed);

        store_last_refresh(&state, repo);
        assert!(!refresh_status(&state, repo).failed);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_failure_before_any_success_reports_no_timestamp() {
        let state = state_dir("never");
        let repo = "/repo/one";
        store_refresh_failure(&state, repo);
        let status = refresh_status(&state, repo);
        assert_eq!(status.refreshed_at, None);
        assert!(status.failed);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_deliberate_refresh_waits_longer_than_the_background_one() {
        assert_eq!(FetchPatience::Background.list_timeout(), LIST_TIMEOUT);
        assert_eq!(
            FetchPatience::Interactive.list_timeout(),
            INTERACTIVE_LIST_TIMEOUT
        );
        assert!(INTERACTIVE_LIST_TIMEOUT > LIST_TIMEOUT);
    }

    #[test]
    fn cache_paths_are_repository_and_branch_collision_safe() {
        let state = Path::new("/tmp/state");
        assert_ne!(
            cache_file(state, "/repo/one", "feature/foo"),
            cache_file(state, "/repo/two", "feature/foo")
        );
        assert_ne!(
            cache_file(state, "/repo/one", "feature/foo"),
            cache_file(state, "/repo/one", "feature-foo")
        );
    }
    #[test]
    fn pr_metadata_matches_repository_identity_not_just_head_name() {
        let rows: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([
            {"number": 1, "state": "OPEN", "headRefName": "feature", "headRefOid": "fork-head", "isCrossRepository": true, "headRepository": {"nameWithOwner": "fork/repo"}},
            {"number": 2, "state": "OPEN", "headRefName": "feature", "headRefOid": "remote-head", "isCrossRepository": false, "headRepository": {"nameWithOwner": "origin/repo"}}
        ])).unwrap();
        let candidates: Vec<_> = rows.iter().collect();
        // Same-repo unpublished commits still receive their open PR metadata.
        assert_eq!(
            super::select_pr_candidate(&candidates, "local-unpublished", None)
                .unwrap()
                .number,
            Some(2)
        );
        assert_eq!(
            super::select_pr_candidate(&candidates, "fork-head", Some("fork/repo"))
                .unwrap()
                .number,
            Some(1)
        );
        assert!(
            super::select_pr_candidate(&candidates, "fork-head", Some("unrelated/repo")).is_none()
        );
        assert!(super::select_pr_candidate(&[&rows[0]], "fork-head", None).is_none());
        let unknown = serde_json::json!({"number":3, "state":"OPEN"});
        assert!(super::select_pr_candidate(&[&unknown], "head", None).is_none());
    }
}
