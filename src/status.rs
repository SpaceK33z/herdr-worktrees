//! Merge/squash/ahead-behind status for a branch relative to the base, plus the
//! per-commit cache.

use crate::git;
use crate::util;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrStatus {
    None,
    Unmerged,
    Merged,
}

/// Compute `(kind, text)` for a branch vs the base. `kind` is one of
/// `merged`, `squashed`, `ahead`, `behind`, `diverged`; `text` is the display
/// string (`merged`, `squashed`, `↑N`, `↓N`, `↑N ↓M`).
pub fn compute_status(
    branch: &str,
    base: &str,
    head: &str,
    use_cache: bool,
    repo: &str,
    state_dir: &Path,
    pr_status: PrStatus,
) -> (String, String) {
    // GitHub PR state takes precedence over cached local status. An open or
    // closed-unmerged PR with no commits of its own must not look merged merely
    // because its branch is equal to or behind the base.
    match pr_status {
        PrStatus::Merged => {
            cache_store(state_dir, branch, head, base, "merged", "merged");
            return ("merged".to_string(), "merged".to_string());
        }
        PrStatus::Unmerged => {
            let (behind, ahead) = left_right_count(repo, base, branch);
            if ahead == 0 {
                return if behind > 0 {
                    ("behind".to_string(), format!("↓{behind}"))
                } else {
                    ("other".to_string(), "—".to_string())
                };
            }
        }
        PrStatus::None => {}
    }

    if use_cache {
        if let Some((k, t)) = cache_get(state_dir, branch, head, base) {
            return (k, t);
        }
    }

    // Ancestor check catches normal fast-forward/merge commits.
    if git::ref_exists(repo, branch)
        && git::ref_exists(repo, base)
        && git::git_success(&["-C", repo, "merge-base", "--is-ancestor", branch, base])
    {
        cache_store(state_dir, branch, head, base, "merged", "merged");
        return ("merged".to_string(), "merged".to_string());
    }

    // Squash/rebase detection: replay the branch's tree onto the merge base as a
    // synthetic commit, then ask `git cherry` whether that patch already exists.
    if let Some(m) = merge_base(repo, branch, base) {
        if let Some(tree) = rev_parse_tree(repo, branch) {
            if let Some(synth) = commit_tree(repo, &tree, &m) {
                if cherry_is_minus(repo, base, &synth) {
                    cache_store(state_dir, branch, head, base, "squashed", "squashed");
                    return ("squashed".to_string(), "squashed".to_string());
                }
            }
        }
    }

    let (behind, ahead) = left_right_count(repo, base, branch);
    if ahead > 0 && behind == 0 {
        let text = format!("↑{ahead}");
        cache_store(state_dir, branch, head, base, "ahead", &text);
        ("ahead".to_string(), text)
    } else if behind > 0 && ahead == 0 {
        let text = format!("↓{behind}");
        cache_store(state_dir, branch, head, base, "behind", &text);
        ("behind".to_string(), text)
    } else if ahead == 0 && behind == 0 {
        cache_store(state_dir, branch, head, base, "merged", "merged");
        ("merged".to_string(), "merged".to_string())
    } else {
        let text = format!("↑{ahead} ↓{behind}");
        cache_store(state_dir, branch, head, base, "diverged", &text);
        ("diverged".to_string(), text)
    }
}

fn merge_base(repo: &str, branch: &str, base: &str) -> Option<String> {
    let s = git::git_stdout(&["-C", repo, "merge-base", branch, base]);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn rev_parse_tree(repo: &str, branch: &str) -> Option<String> {
    let spec = format!("{branch}^{{tree}}");
    let s = git::git_stdout(&["-C", repo, "rev-parse", &spec]);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn commit_tree(repo: &str, tree: &str, parent: &str) -> Option<String> {
    let mut child = std::process::Command::new("git")
        .args([
            "-C", repo, "-c", "user.name=herdr-worktrees", "-c",
            "user.email=herdr-worktrees@invalid", "commit-tree", tree, "-p", parent,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .as_mut()?
        .write_all(b"herdr-worktrees squash probe")
        .ok()?;
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// `git cherry` prints `- <sha>` for patches already applied; `+` for new ones.
fn cherry_is_minus(repo: &str, base: &str, synth: &str) -> bool {
    git::git_stdout(&["-C", repo, "cherry", base, synth]).trim_start().starts_with('-')
}

fn left_right_count(repo: &str, base: &str, branch: &str) -> (u32, u32) {
    let spec = format!("{base}...{branch}");
    let out = git::git_stdout(&["-C", repo, "rev-list", "--left-right", "--count", &spec]);
    let mut it = out.split_whitespace();
    let behind = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ahead = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (behind, ahead)
}

fn cache_get(state_dir: &Path, branch: &str, head: &str, base: &str) -> Option<(String, String)> {
    let file = state_dir.join("merge-status-v2").join(util::sanitize(branch));
    let content = std::fs::read_to_string(file).ok()?;
    let mut parts = content.split('\t');
    let ch = parts.next()?;
    let cb = parts.next()?;
    let ck = parts.next()?;
    let ct = parts.next().unwrap_or("");
    if ch == head && cb == base {
        Some((ck.to_string(), ct.to_string()))
    } else {
        None
    }
}

fn cache_store(state_dir: &Path, branch: &str, head: &str, base: &str, kind: &str, text: &str) {
    let dir = state_dir.join("merge-status-v2");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(
            dir.join(util::sanitize(branch)),
            format!("{head}\t{base}\t{kind}\t{text}"),
        );
    }
}
