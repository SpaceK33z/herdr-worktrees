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
}

impl PrInfo {
    pub fn has_pr(&self) -> bool {
        self.number.is_some()
    }
}

const CACHE_TTL_MS: u128 = 60_000;
const PR_LIMIT: usize = 1000;
static CACHE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fetch all requested branch PRs with one `gh` process and one API query.
/// If every branch has a fresh cache entry, no process is started.
pub fn fetch_many(
    branch_heads: &[(String, String)],
    repo: &str,
    state_dir: &Path,
) -> HashMap<String, PrInfo> {
    let mut cached = HashMap::with_capacity(branch_heads.len());
    let mut all_cached = true;
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

    let Some(json) = gh_fetch(repo) else {
        return cached;
    };
    let Some(fetched) = parse_gh_many(&json, branch_heads) else {
        return cached;
    };
    for (branch, info) in fetched {
        cache_store(state_dir, repo, &branch, &info);
        cached.insert(branch, info);
    }
    cached
}

/// Run one repository-wide `gh pr list` with a hard timeout. A timed-out child
/// is killed and reaped instead of lingering after the picker has moved on.
fn gh_fetch(repo: &str) -> Option<String> {
    let limit = PR_LIMIT.to_string();
    let child = std::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "all",
            "--json",
            "number,url,state,isDraft,reviewDecision,mergeStateStatus,headRefName,headRefOid",
            "--limit",
            &limit,
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

fn parse_gh_many(json: &str, branch_heads: &[(String, String)]) -> Option<HashMap<String, PrInfo>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let prs = value.as_array()?;
    let complete = prs.len() < PR_LIMIT;
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
    })
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u128,
    repo: String,
    branch: String,
    info: PrInfo,
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
    state_dir
        .join("pr-info-v2")
        .join(format!("{:016x}", cache_hash(repo)))
        .join(format!("{:016x}.json", cache_hash(branch)))
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
    use super::{cache_file, parse_gh, parse_gh_many, PR_LIMIT};
    use std::path::Path;

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
        let prs = parse_gh_many(json, &branches).unwrap();
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
        assert!(!parse_gh_many(&json, &requested)
            .unwrap()
            .contains_key("missing"));
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
