//! Fetch GitHub PR data for a branch (`gh pr list`), with a short-TTL cache.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    pub url: Option<String>,
}

/// Resolve a pull request by number via `gh pr view`. An explicit user action,
/// so it gets a longer timeout than the background picker cache and captures
/// stderr for diagnostics. Returns `None` when `gh` is missing, unauthenticated,
/// or the PR does not exist.
pub fn resolve_pr(number: u32, repo: &str) -> Option<PullRequestTarget> {
    let fields = "number,headRefName,headRefOid,baseRefName,isCrossRepository,url";
    let child = std::process::Command::new("gh")
        .args(["pr", "view", &number.to_string(), "--json", fields])
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let output = match rx.recv_timeout(std::time::Duration::from_millis(5000)) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return None,
        Err(_) => {
            // SAFETY: `pid` came from the still-running child. The waiter owns
            // the Child and will reap it after SIGKILL.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            return None;
        }
    };
    if !output.status.success() {
        return None;
    }
    parse_pr_target(&String::from_utf8_lossy(&output.stdout))
}

fn parse_pr_target(json: &str) -> Option<PullRequestTarget> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(PullRequestTarget {
        number: value.get("number")?.as_u64()? as u32,
        head_ref: value.get("headRefName")?.as_str()?.to_string(),
        head_oid: value.get("headRefOid")?.as_str()?.to_string(),
        base_ref: value.get("baseRefName")?.as_str()?.to_string(),
        is_cross_repo: value.get("isCrossRepository")?.as_bool().unwrap_or(false),
        url: value.get("url").and_then(|u| u.as_str()).map(String::from),
    })
}

const CACHE_TTL_MS: u128 = 60_000;
const PR_LIMIT: usize = 1000;
const THREAD_LIMIT: usize = 100;
static CACHE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fetch all requested branch PRs with one `gh` process and one API query.
/// If every branch has a fresh cache entry, no process is started.
pub fn fetch_many(
    branch_heads: &[(String, String)],
    repo: &str,
    state_dir: &Path,
    use_cache: bool,
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

    let fetched = gh_fetch(repo)
        .and_then(|json| parse_gh_many(&json, branch_heads, true))
        // Large repositories can make the all-history query exceed the picker's
        // timeout. Fall back to the much cheaper open-only query so actionable
        // PR numbers still appear. Missing branches are not cached because they
        // may have a merged PR that this response cannot prove absent.
        .or_else(|| gh_fetch_open(repo).and_then(|json| parse_gh_many(&json, branch_heads, false)));
    let Some(mut fetched) = fetched else {
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
    for (branch, info) in fetched {
        cache_store(state_dir, repo, &branch, &info);
        cached.insert(branch, info);
    }
    cached
}

/// Run one repository-wide `gh pr list` with a hard timeout. A timed-out child
/// is killed and reaped instead of lingering after the picker has moved on.
fn gh_fetch(repo: &str) -> Option<String> {
    gh_fetch_state(
        repo,
        "all",
        "id,number,url,state,isDraft,reviewDecision,mergeStateStatus,headRefName,headRefOid",
    )
}

fn gh_fetch_open(repo: &str) -> Option<String> {
    gh_fetch_state(
        repo,
        "open",
        "id,number,url,state,isDraft,reviewDecision,headRefName,headRefOid",
    )
}

fn gh_fetch_state(repo: &str, state: &str, fields: &str) -> Option<String> {
    let limit = PR_LIMIT.to_string();
    let child = std::process::Command::new("gh")
        .args([
            "pr", "list", "--state", state, "--json", fields, "--limit", &limit,
        ])
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let output = match rx.recv_timeout(std::time::Duration::from_millis(1500)) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return None,
        Err(_) => {
            // SAFETY: `pid` came from the still-running child. The waiter owns
            // the Child and will reap it after SIGKILL.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            return None;
        }
    };
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
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
    let child = std::process::Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(std::time::Duration::from_millis(2000)) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return None,
        Err(_) => {
            // SAFETY: `pid` came from the still-running child. The waiter owns
            // the Child and will reap it after SIGKILL.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            return None;
        }
    };
    if !output.status.success() {
        return None;
    }
    parse_thread_counts(&String::from_utf8_lossy(&output.stdout))
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
    ts: u128,
    repo: String,
}

pub fn last_refresh_ms(state_dir: &Path, repo: &str) -> Option<u128> {
    let content = std::fs::read_to_string(refresh_file(state_dir, repo)).ok()?;
    let entry: RefreshEntry = serde_json::from_str(&content).ok()?;
    (entry.repo == repo).then_some(entry.ts)
}

fn store_last_refresh(state_dir: &Path, repo: &str) {
    let file = refresh_file(state_dir, repo);
    let Some(dir) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let entry = RefreshEntry {
        ts: now_ms(),
        repo: repo.to_string(),
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    let nonce = CACHE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = file.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| file.write_all(json.as_bytes()))
        .is_ok();
    if written {
        let _ = std::fs::rename(&temp, file);
    } else {
        let _ = std::fs::remove_file(temp);
    }
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
    let file = cache_file(state_dir, repo, branch);
    let Some(dir) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let entry = CacheEntry {
        ts: now_ms(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        info: info.clone(),
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    let nonce = CACHE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = file.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| file.write_all(json.as_bytes()))
        .is_ok();
    if written {
        let _ = std::fs::rename(&temp, file);
    } else {
        let _ = std::fs::remove_file(temp);
    }
}

fn cache_file(state_dir: &Path, repo: &str, branch: &str) -> PathBuf {
    repo_cache_dir(state_dir, repo).join(format!("{:016x}.json", cache_hash(branch)))
}

fn refresh_file(state_dir: &Path, repo: &str) -> PathBuf {
    repo_cache_dir(state_dir, repo).join("last-refresh.json")
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
        cache_file, parse_gh, parse_gh_many, parse_pr_target, parse_thread_counts, ThreadCount,
        PR_LIMIT,
    };
    use std::path::Path;

    #[test]
    fn pr_target_parses_head_base_and_cross_repo_flag() {
        let json = r#"{
            "number": 42,
            "headRefName": "kees/fix",
            "headRefOid": "abc123",
            "baseRefName": "main",
            "isCrossRepository": true,
            "url": "https://github.com/o/r/pull/42"
        }"#;
        let target = parse_pr_target(json).unwrap();
        assert_eq!(target.number, 42);
        assert_eq!(target.head_ref, "kees/fix");
        assert_eq!(target.head_oid, "abc123");
        assert_eq!(target.base_ref, "main");
        assert!(target.is_cross_repo);
        assert_eq!(target.url.as_deref(), Some("https://github.com/o/r/pull/42"));
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
