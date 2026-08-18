//! Fetch GitHub PR data for a branch (`gh pr list`), with a short-TTL cache.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: Option<u32>,
    pub url: Option<String>,
    pub is_draft: bool,
    pub review: Option<String>,
    pub merged: bool,
    pub conflict: bool,
}

impl PrInfo {
    pub fn has_pr(&self) -> bool {
        self.number.is_some()
    }
}

const CACHE_TTL_MS: u128 = 60_000;

/// Fetch PR info for `branch`, using the short-TTL cache when fresh.
pub fn fetch(branch: &str, repo: &str, state_dir: &Path) -> Option<PrInfo> {
    if let Some(cached) = cache_get(state_dir, branch) {
        return Some(cached);
    }
    let info = gh_fetch(branch, repo);
    if let Some(info) = &info {
        cache_store(state_dir, branch, info);
    }
    info
}

/// Run `gh pr list` in the background and give it a short window. Returns
/// `None` on timeout or error (never block status on a slow network call).
fn gh_fetch(branch: &str, repo: &str) -> Option<PrInfo> {
    let (tx, rx) = std::sync::mpsc::channel();
    let branch = branch.to_string();
    let repo = repo.to_string();
    std::thread::spawn(move || {
        let result = std::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                &branch,
                "--state",
                "all",
                "--json",
                "number,url,state,isDraft,reviewDecision,mergeStateStatus",
                "--limit",
                "1",
            ])
            .current_dir(&repo)
            .output();
        let _ = tx.send(result);
    });

    let out = match rx.recv_timeout(std::time::Duration::from_millis(1500)) {
        Ok(Ok(out)) => out,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    parse_gh(&String::from_utf8_lossy(&out.stdout))
}

fn parse_gh(json: &str) -> Option<PrInfo> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = v.as_array()?;
    // `--limit 1`; an empty array means "no PR for this branch" (still valid).
    let Some(pr) = arr.first() else {
        return Some(PrInfo::default());
    };
    let state = pr.get("state").and_then(|s| s.as_str()).unwrap_or("");
    Some(PrInfo {
        number: pr
            .get("number")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
        url: pr.get("url").and_then(|u| u.as_str()).map(String::from),
        is_draft: pr.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false),
        review: pr
            .get("reviewDecision")
            .and_then(|r| r.as_str())
            .map(String::from),
        merged: state == "MERGED",
        conflict: pr.get("mergeStateStatus").and_then(|m| m.as_str()) == Some("DIRTY"),
    })
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u128,
    info: PrInfo,
}

fn cache_get(state_dir: &Path, branch: &str) -> Option<PrInfo> {
    let file = state_dir
        .join("pr-info-v1")
        .join(crate::util::sanitize(branch));
    let content = std::fs::read_to_string(file).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;
    if now_ms() < entry.ts.saturating_add(CACHE_TTL_MS) {
        Some(entry.info)
    } else {
        None
    }
}

fn cache_store(state_dir: &Path, branch: &str, info: &PrInfo) {
    let dir = state_dir.join("pr-info-v1");
    if std::fs::create_dir_all(&dir).is_ok() {
        let entry = CacheEntry {
            ts: now_ms(),
            info: info.clone(),
        };
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = std::fs::write(dir.join(crate::util::sanitize(branch)), json);
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
