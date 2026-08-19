//! Thin wrappers around `git` (the plugin shells out to plain git everywhere).

use crate::config::Config;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Output;

pub fn git_output(args: &[&str]) -> std::io::Result<Output> {
    std::process::Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .output()
}

pub fn git_stdout(args: &[&str]) -> String {
    git_output(args)
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

pub fn git_success(args: &[&str]) -> bool {
    git_output(args)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git` with inherited stdio so errors and progress reach the terminal.
/// Used for explicit user actions (fetch, worktree add) where the user should
/// see why something failed.
pub fn git_inherit(args: &[&str]) -> bool {
    std::process::Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The commit hash a ref points to, if the ref exists.
pub fn ref_oid(repo: &str, refname: &str) -> Option<String> {
    let oid = git_stdout(&["-C", repo, "rev-parse", "-q", "--verify", refname]);
    let oid = oid.trim();
    (!oid.is_empty()).then(|| oid.to_string())
}

/// Delete a ref (used to clean up temporary PR refs).
pub fn delete_ref(repo: &str, refname: &str) -> bool {
    git_success(&["-C", repo, "update-ref", "-d", refname])
}

/// The main working-tree path (the repo root), not the current worktree.
pub fn repo_root() -> Result<PathBuf> {
    let cdir = git_stdout(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let cdir = cdir.trim();
    if cdir.is_empty() {
        anyhow::bail!("not inside a git repository");
    }
    let p = PathBuf::from(cdir);
    if p.file_name()
        .map(|f| f.to_string_lossy() == ".git")
        .unwrap_or(false)
    {
        Ok(p.parent().map(Path::to_path_buf).unwrap_or(p))
    } else {
        Ok(p)
    }
}

pub fn resolve_user(repo: &str) -> String {
    let u = git_stdout(&["-C", repo, "config", "--get", "user.name"]);
    let u = u.trim();
    if !u.is_empty() {
        return u.to_string();
    }
    if let Ok(u) = std::env::var("USER") {
        if !u.is_empty() {
            return u;
        }
    }
    if let Ok(out) = std::process::Command::new("id").args(["-un"]).output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "unknown".to_string()
}

/// The branch of the process's current directory (the pane the popup opened in).
pub fn current_branch() -> String {
    git_stdout(&["branch", "--show-current"]).trim().to_string()
}

/// The top-level path of the current worktree (the process's cwd).
pub fn current_toplevel() -> String {
    git_stdout(&["rev-parse", "--show-toplevel"])
        .trim()
        .to_string()
}

pub fn ref_exists(repo: &str, refname: &str) -> bool {
    git_success(&["-C", repo, "rev-parse", "-q", "--verify", refname])
}

/// Resolve the full remote-tracking ref used to measure push/pull state.
/// Prefer the configured upstream, then fall back to `origin/<branch>` when
/// that ref exists (useful when a branch was pushed without `--set-upstream`).
pub fn branch_upstream(repo: &str, branch: &str) -> Option<String> {
    let local_ref = format!("refs/heads/{branch}");
    let configured = git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(upstream)",
        &local_ref,
    ]);
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }

    let origin = format!("refs/remotes/origin/{branch}");
    ref_exists(repo, &origin).then_some(origin)
}

/// Resolve the base ref used when creating a worktree. Prefer the remote-
/// tracking branch so new work starts from the latest fetched base rather than
/// a potentially stale local checkout.
pub fn resolve_base_ref(config: &Config, repo: &str, current_branch: &str) -> String {
    if let Some(cfg) = &config.base_branch {
        if !cfg.is_empty() {
            if ref_exists(repo, &format!("refs/remotes/origin/{cfg}")) {
                return format!("origin/{cfg}");
            }
            if ref_exists(repo, &format!("refs/heads/{cfg}")) {
                return cfg.clone();
            }
        }
    }

    let rh = git_stdout(&["-C", repo, "symbolic-ref", "-q", "refs/remotes/origin/HEAD"]);
    let rh = rh.trim();
    if !rh.is_empty() {
        let rh = rh.strip_prefix("refs/remotes/").unwrap_or(rh).to_string();
        if ref_exists(repo, &format!("refs/remotes/{rh}")) {
            return rh;
        }
        let local = rh.strip_prefix("origin/").unwrap_or(&rh).to_string();
        if ref_exists(repo, &format!("refs/heads/{local}")) {
            return local;
        }
    }

    for c in ["main", "master"] {
        if ref_exists(repo, &format!("refs/remotes/origin/{c}")) {
            return format!("origin/{c}");
        }
    }
    for c in ["main", "master"] {
        if ref_exists(repo, &format!("refs/heads/{c}")) {
            return c.to_string();
        }
    }

    current_branch.to_string()
}

pub fn has_github_remote(repo: &str) -> bool {
    let out = git_stdout(&["-C", repo, "remote", "-v"]);
    out.to_lowercase().contains("github.com")
}
