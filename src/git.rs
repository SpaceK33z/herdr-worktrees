//! Thin wrappers around `git` (the plugin shells out to plain git everywhere).

use crate::config::Config;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

/// How long a pre-create `git fetch` may run before it is abandoned. Creating
/// a worktree is interactive, so a slow or unreachable remote must not hold the
/// popup open indefinitely.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub fn git_output(args: &[&str]) -> std::io::Result<Output> {
    std::process::Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .output()
}

/// Stdout of a `git` command, lossily decoded.
///
/// A command that fails to spawn, exits non-zero, or writes only to stderr all
/// give the same empty string: callers use this for queries where "no output"
/// and "no answer" mean the same thing (no refs, no worktrees, no config). When
/// a failure has to be told apart from an empty result, use [`git_output`] or
/// [`git_success`].
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

/// Stream `git` output to stderr so errors and progress reach the terminal
/// without contaminating the caller's machine-readable stdout. Used for
/// explicit user actions (fetch, worktree add).
pub fn git_inherit(args: &[&str]) -> bool {
    std::process::Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Run `git` with stdio detached, giving up (and killing the child) after
/// `timeout`. Used for the network calls that must never block the popup.
pub fn git_timeout(args: &[&str], timeout: Duration) -> bool {
    let child = std::process::Command::new("git")
        .env("LC_ALL", "C")
        // Never stop on a credential prompt: nobody is there to answer it.
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Split a base ref into `(remote, branch)` when it names a remote-tracking
/// branch (`origin/main`, `upstream/release/1.x`). Local branch names — and
/// remote-looking names whose remote does not exist — yield `None`.
pub fn remote_base_parts(repo: &str, base: &str) -> Option<(String, String)> {
    let (remote, branch) = base.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        return None;
    }
    if !ref_exists(repo, &format!("refs/remotes/{base}")) {
        return None;
    }
    let known = git_stdout(&["-C", repo, "remote"])
        .lines()
        .any(|line| line.trim() == remote);
    known.then(|| (remote.to_string(), branch.to_string()))
}

/// Refresh the remote-tracking ref behind `base` so new work starts from the
/// current upstream tip instead of whatever was fetched last. Best effort: the
/// caller continues with the existing (possibly stale) ref when this returns
/// `false`, and local bases are skipped entirely.
pub fn fetch_base(repo: &str, base: &str) -> FetchBase {
    let Some((remote, branch)) = remote_base_parts(repo, base) else {
        return FetchBase::Skipped;
    };
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    let ok = git_timeout(
        &[
            "-C",
            repo,
            "fetch",
            "--quiet",
            "--no-tags",
            &remote,
            &refspec,
        ],
        FETCH_TIMEOUT,
    );
    if ok {
        FetchBase::Updated
    } else {
        FetchBase::Failed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchBase {
    /// `base` is not a remote-tracking branch; there is nothing to refresh.
    Skipped,
    Updated,
    /// The remote was unreachable, rejected the fetch, or took too long.
    Failed,
}

/// The commit hash a ref points to, if the ref exists.
pub fn ref_oid(repo: &str, refname: &str) -> Option<String> {
    let output = git_output(&["-C", repo, "rev-parse", "-q", "--verify", refname]).ok()?;
    if !output.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&output.stdout);
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
        // A full name like "Kees Kluskens" must still work as a branch prefix
        // and a path fragment, so collapse it to one safe token first.
        return crate::util::sanitize(u);
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

/// The base-ref precedence ladder, over abstract ref lookups: the configured
/// base branch (remote-tracking copy first), then `origin/HEAD`, then
/// `main`/`master` on origin, then locally, and finally the current branch.
///
/// `has_ref` answers full ref names (`refs/heads/x`, `refs/remotes/origin/x`)
/// and `origin_head` yields the `refs/remotes/origin/HEAD` symref target, so
/// both a live repository and a batched ref snapshot can supply the lookups.
pub fn base_ref_policy(
    config: &Config,
    current_branch: &str,
    has_ref: impl Fn(&str) -> bool,
    origin_head: impl FnOnce() -> Option<String>,
) -> String {
    if let Some(cfg) = config
        .base_branch
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        if has_ref(&format!("refs/remotes/origin/{cfg}")) {
            return format!("origin/{cfg}");
        }
        if has_ref(&format!("refs/heads/{cfg}")) {
            return cfg.to_string();
        }
    }

    if let Some(head) = origin_head() {
        let remote = head.strip_prefix("refs/remotes/").unwrap_or(&head);
        if has_ref(&format!("refs/remotes/{remote}")) {
            return remote.to_string();
        }
        let local = remote.strip_prefix("origin/").unwrap_or(remote);
        if has_ref(&format!("refs/heads/{local}")) {
            return local.to_string();
        }
    }

    for c in ["main", "master"] {
        if has_ref(&format!("refs/remotes/origin/{c}")) {
            return format!("origin/{c}");
        }
    }
    for c in ["main", "master"] {
        if has_ref(&format!("refs/heads/{c}")) {
            return c.to_string();
        }
    }

    current_branch.to_string()
}

/// Resolve the base ref used when creating a worktree. Prefer the remote-
/// tracking branch so new work starts from the latest fetched base rather than
/// a potentially stale local checkout.
pub fn resolve_base_ref(config: &Config, repo: &str, current_branch: &str) -> String {
    base_ref_policy(
        config,
        current_branch,
        |refname| ref_exists(repo, refname),
        || {
            let head = git_stdout(&["-C", repo, "symbolic-ref", "-q", "refs/remotes/origin/HEAD"]);
            let head = head.trim();
            (!head.is_empty()).then(|| head.to_string())
        },
    )
}

pub fn has_github_remote(repo: &str) -> bool {
    let out = git_stdout(&["-C", repo, "remote", "-v"]);
    out.to_lowercase().contains("github.com")
}

#[cfg(test)]
mod tests {
    use super::{
        base_ref_policy, fetch_base, git_timeout, ref_oid, remote_base_parts, resolve_user,
        FetchBase,
    };
    use crate::config::Config;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn config(base_branch: Option<&str>) -> Config {
        let toml = base_branch.map_or_else(String::new, |name| format!("base-branch = {name:?}"));
        toml::from_str(&toml).expect("test config to parse")
    }

    fn resolve(base_branch: Option<&str>, refs: &[&str], origin_head: Option<&str>) -> String {
        base_ref_policy(
            &config(base_branch),
            "current",
            |refname| refs.contains(&refname),
            || origin_head.map(str::to_string),
        )
    }

    #[test]
    fn configured_base_prefers_the_remote_tracking_copy() {
        let refs = ["refs/heads/trunk", "refs/remotes/origin/trunk"];
        assert_eq!(resolve(Some("trunk"), &refs, None), "origin/trunk");
        assert_eq!(resolve(Some("trunk"), &["refs/heads/trunk"], None), "trunk");
    }

    #[test]
    fn empty_or_missing_configured_base_falls_through() {
        let refs = ["refs/remotes/origin/main"];
        assert_eq!(resolve(Some(""), &refs, None), "origin/main");
        assert_eq!(resolve(Some("gone"), &refs, None), "origin/main");
    }

    #[test]
    fn origin_head_outranks_the_main_master_guesses() {
        let refs = ["refs/remotes/origin/main", "refs/remotes/origin/dev"];
        assert_eq!(
            resolve(None, &refs, Some("refs/remotes/origin/dev")),
            "origin/dev"
        );
        // A symref pointing at a branch that only exists locally.
        assert_eq!(
            resolve(
                None,
                &["refs/heads/dev", "refs/heads/main"],
                Some("refs/remotes/origin/dev")
            ),
            "dev"
        );
    }

    #[test]
    fn falls_back_to_main_then_master_then_the_current_branch() {
        assert_eq!(
            resolve(None, &["refs/remotes/origin/master"], None),
            "origin/master"
        );
        assert_eq!(
            resolve(
                None,
                &["refs/heads/master", "refs/remotes/origin/other"],
                None
            ),
            "master"
        );
        assert_eq!(
            resolve(None, &["refs/heads/main", "refs/heads/master"], None),
            "main"
        );
        assert_eq!(resolve(None, &[], None), "current");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .args(args)
            .output()
            .expect("git to run");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// An `origin` with one commit on `main`, plus a clone of it.
    fn clone_with_remote(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "herdr-wt-fetch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin");
        let clone = root.join("clone");
        std::fs::create_dir_all(&origin).unwrap();
        git(&root, &["init", "-q", "-b", "main", "origin"]);
        std::fs::write(origin.join("f"), "one").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-qm", "one"]);
        git(
            &root,
            &[
                "clone",
                "-q",
                &origin.to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        );
        (origin, clone)
    }

    #[test]
    fn remote_base_parts_only_matches_real_remote_tracking_refs() {
        let (_origin, clone) = clone_with_remote("parts");
        let repo = clone.to_string_lossy();
        assert_eq!(
            remote_base_parts(&repo, "origin/main"),
            Some(("origin".to_string(), "main".to_string()))
        );
        // A local branch, a slash-free name, and an unknown remote all skip.
        assert_eq!(remote_base_parts(&repo, "main"), None);
        assert_eq!(remote_base_parts(&repo, "upstream/main"), None);
    }

    #[test]
    fn fetch_base_advances_the_remote_tracking_ref() {
        let (origin, clone) = clone_with_remote("advance");
        let repo = clone.to_string_lossy().into_owned();
        let before = ref_oid(&repo, "refs/remotes/origin/main").unwrap();

        std::fs::write(origin.join("f"), "two").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-qm", "two"]);
        let head = git(&origin, &["rev-parse", "HEAD"]);
        // Untouched until we fetch.
        assert_eq!(ref_oid(&repo, "refs/remotes/origin/main"), Some(before));

        assert_eq!(fetch_base(&repo, "origin/main"), FetchBase::Updated);
        assert_eq!(ref_oid(&repo, "refs/remotes/origin/main"), Some(head));

        // A local base has no remote counterpart to refresh.
        assert_eq!(fetch_base(&repo, "main"), FetchBase::Skipped);
    }

    #[test]
    fn fetch_base_reports_failure_without_touching_the_stale_ref() {
        let (origin, clone) = clone_with_remote("unreachable");
        let repo = clone.to_string_lossy().into_owned();
        let stale = ref_oid(&repo, "refs/remotes/origin/main").unwrap();
        std::fs::remove_dir_all(&origin).unwrap();

        assert_eq!(fetch_base(&repo, "origin/main"), FetchBase::Failed);
        assert_eq!(ref_oid(&repo, "refs/remotes/origin/main"), Some(stale));
    }

    #[test]
    fn a_multi_word_user_name_becomes_one_safe_token() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-wt-user-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "user.name", "Kees Kluskens"]);
        let repo = dir.to_string_lossy();
        // The name must survive as a branch prefix: no spaces, no slashes.
        assert_eq!(resolve_user(&repo), "Kees-Kluskens");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_timeout_kills_a_command_that_outruns_its_deadline() {
        let started = Instant::now();
        // The `ext::` transport runs an arbitrary helper, so this is a real git
        // network call that simply never answers.
        let ok = git_timeout(
            &[
                "-c",
                "protocol.ext.allow=always",
                "ls-remote",
                "ext::sleep 30",
            ],
            Duration::from_millis(300),
        );
        assert!(!ok);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
