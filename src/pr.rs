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
    let fields = "number,headRefName,headRefOid,baseRefName,isCrossRepository";
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
        is_cross_repo: value.get("isCrossRepository")?.as_bool().unwrap_or(false),
    })
}

const CACHE_TTL_MS: u128 = 60_000;
const PR_LIMIT: usize = 1000;
const THREAD_LIMIT: usize = 100;
/// Cache files untouched for this long are deleted on the next refresh.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const REFRESH_FILE: &str = "last-refresh.json";

/// Fetch all requested branch PRs with one `gh` process and one API query.
/// If every branch has a fresh cache entry, no process is started.
pub fn fetch_many(
    branch_heads: &[(String, String)],
    repo: &str,
    state_dir: &Path,
    use_cache: bool,
    patience: FetchPatience,
) -> HashMap<String, PrInfo> {
    let mut cached = HashMap::with_capacity(branch_heads.len());
    let mut all_cached = use_cache;
    for (branch, _) in branch_heads {
        match cache_get(state_dir, repo, branch) {
            Some(info) => {
                cached.insert(branch.clone(), info);
            }
            None => all_cached = false,
        }
    }
    if all_cached || branch_heads.is_empty() {
        return cached;
    }

    let fetched = gh_fetch(repo, patience)
        .and_then(|json| parse_gh_many(&json, branch_heads, true))
        // Large repositories can make the all-history query exceed the picker's
        // timeout. Fall back to the much cheaper open-only query so actionable
        // PR numbers still appear. Missing branches are not cached because they
        // may have a merged PR that this response cannot prove absent.
        .or_else(|| {
            gh_fetch_open(repo, patience).and_then(|json| parse_gh_many(&json, branch_heads, false))
        });
    let Some(mut fetched) = fetched else {
        // Empty PR columns otherwise look like "this repository has no pull
        // requests"; record the failure so the picker footer can say so.
        store_refresh_failure(state_dir, repo);
        return cached;
    };
    let node_ids: Vec<_> = fetched
        .values()
        .filter(|info| info.has_pr() && !info.merged)
        .filter_map(|info| info.node_id.clone())
        .collect();
    if let Some(thread_counts) = gh_fetch_thread_counts(repo, &node_ids) {
        for info in fetched.values_mut() {
            if let Some(count) = info.node_id.as_deref().and_then(|id| thread_counts.get(id)) {
                info.unresolved_threads = Some(count.unresolved);
                info.threads_truncated = count.truncated;
            }
        }
    }
    store_last_refresh(state_dir, repo);
    prune_stale_cache(state_dir, repo);
    for (branch, info) in fetched {
        cache_store(state_dir, repo, &branch, &info);
        cached.insert(branch, info);
    }
    cached
}

/// Run one repository-wide `gh pr list` with a hard timeout. A timed-out child
/// is killed and reaped instead of lingering after the picker has moved on.
fn gh_fetch(repo: &str, patience: FetchPatience) -> Option<String> {
    gh_fetch_state(
        repo,
        "all",
        "id,number,url,state,isDraft,reviewDecision,mergeStateStatus,headRefName,headRefOid",
        patience,
    )
}

fn gh_fetch_open(repo: &str, patience: FetchPatience) -> Option<String> {
    gh_fetch_state(
        repo,
        "open",
        "id,number,url,state,isDraft,reviewDecision,headRefName,headRefOid",
        patience,
    )
}

fn gh_fetch_state(
    repo: &str,
    state: &str,
    fields: &str,
    patience: FetchPatience,
) -> Option<String> {
    let limit = PR_LIMIT.to_string();
    gh_output(
        repo,
        &[
            "pr", "list", "--state", state, "--json", fields, "--limit", &limit,
        ],
        patience.list_timeout(),
        false,
    )
    .ok()
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

fn parse_gh_many(
    json: &str,
    branch_heads: &[(String, String)],
    includes_closed: bool,
) -> Option<HashMap<String, PrInfo>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let prs = value.as_array()?;
    let complete = includes_closed && prs.len() < PR_LIMIT;
    let mut by_branch: HashMap<&str, Vec<&serde_json::Value>> = HashMap::new();
    for pr in prs {
        if let Some(branch) = pr.get("headRefName").and_then(|name| name.as_str()) {
            by_branch.entry(branch).or_default().push(pr);
        }
    }

    let mut result = HashMap::with_capacity(branch_heads.len());
    for (branch, head) in branch_heads {
        let selected = by_branch
            .get(branch.as_str())
            .and_then(|candidates| select_pr_candidate(candidates, head));
        if let Some(info) = selected {
            result.insert(branch.clone(), info);
        } else if complete {
            result.insert(branch.clone(), PrInfo::default());
        }
    }
    Some(result)
}

#[cfg(test)]
fn parse_gh(json: &str, current_head: &str) -> Option<PrInfo> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let prs: Vec<_> = value.as_array()?.iter().collect();
    Some(select_pr(&prs, current_head))
}

#[cfg(test)]
fn select_pr(prs: &[&serde_json::Value], current_head: &str) -> PrInfo {
    select_pr_candidate(prs, current_head).unwrap_or_default()
}

fn select_pr_candidate(prs: &[&serde_json::Value], current_head: &str) -> Option<PrInfo> {
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
    repo_cache_dir(state_dir, repo).join(format!("{:016x}.json", cache_hash(branch)))
}

fn refresh_file(state_dir: &Path, repo: &str) -> PathBuf {
    repo_cache_dir(state_dir, repo).join(REFRESH_FILE)
}

fn repo_cache_dir(state_dir: &Path, repo: &str) -> PathBuf {
    state_dir
        .join("pr-info-v2")
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
        cache_file, cache_store, cached_many, first_line, now_ms, parse_gh, parse_gh_many,
        parse_pr_target, parse_thread_counts, prune_stale_cache, refresh_file, refresh_status,
        repo_cache_dir, run_with_timeout, store_last_refresh, store_refresh_failure, CacheEntry,
        FetchPatience, GhError, PrInfo, ThreadCount, CACHE_MAX_AGE, CACHE_TTL_MS,
        INTERACTIVE_LIST_TIMEOUT, LIST_TIMEOUT, PR_LIMIT,
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
            "isCrossRepository": true
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
            {"number":1,"state":"MERGED","headRefOid":"head","mergeStateStatus":"CLEAN"},
            {"number":2,"state":"OPEN","headRefOid":"head","mergeStateStatus":"DIRTY"}
        ]"#;
        let pr = parse_gh(json, "head").unwrap();
        assert_eq!(pr.number, Some(2));
        assert!(!pr.merged);
        assert!(pr.conflict);
    }

    #[test]
    fn merged_pr_only_applies_to_the_current_head() {
        let json = r#"[
            {"number":1,"state":"MERGED","headRefOid":"merged-head","mergeStateStatus":"CLEAN"}
        ]"#;
        let merged = parse_gh(json, "merged-head").unwrap();
        assert_eq!(merged.number, Some(1));
        assert!(merged.merged);

        let stale = parse_gh(json, "new-local-head").unwrap();
        assert_eq!(stale.number, None);
        assert!(!stale.merged);
    }

    #[test]
    fn repository_wide_response_is_grouped_by_branch() {
        let json = r#"[
            {"number":1,"state":"OPEN","headRefName":"one","headRefOid":"one-head"},
            {"number":2,"state":"MERGED","headRefName":"two","headRefOid":"two-head"},
            {"number":3,"state":"OPEN","headRefName":"other","headRefOid":"other-head"}
        ]"#;
        let branches = vec![
            ("one".to_string(), "one-head".to_string()),
            ("two".to_string(), "two-head".to_string()),
            ("none".to_string(), "none-head".to_string()),
        ];
        let prs = parse_gh_many(json, &branches, true).unwrap();
        assert_eq!(prs["one"].number, Some(1));
        assert_eq!(prs["two"].number, Some(2));
        assert!(prs["two"].merged);
        assert_eq!(prs["none"].number, None);
    }

    #[test]
    fn saturated_response_does_not_cache_false_negative() {
        let prs: Vec<_> = (0..PR_LIMIT)
            .map(|index| {
                serde_json::json!({
                    "number": index,
                    "state": if index == 0 { "CLOSED" } else { "OPEN" },
                    "headRefName": if index == 0 {
                        "missing".to_string()
                    } else {
                        format!("other-{index}")
                    },
                    "headRefOid": "head"
                })
            })
            .collect();
        let json = serde_json::to_string(&prs).unwrap();
        let requested = vec![("missing".to_string(), "head".to_string())];
        assert!(!parse_gh_many(&json, &requested, true)
            .unwrap()
            .contains_key("missing"));
    }

    #[test]
    fn open_only_response_does_not_cache_merged_branches_as_missing() {
        let json = r#"[
            {"number":2,"state":"OPEN","headRefName":"open","headRefOid":"open-head"}
        ]"#;
        let branches = vec![
            ("open".to_string(), "open-head".to_string()),
            ("possibly-merged".to_string(), "merged-head".to_string()),
        ];
        let prs = parse_gh_many(json, &branches, false).unwrap();
        assert_eq!(prs["open"].number, Some(2));
        assert!(!prs.contains_key("possibly-merged"));
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
}
