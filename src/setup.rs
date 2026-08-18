//! Run the configured `[pre-start] setup-worktree` script in a new checkout.

use crate::config::Config;
use crate::util;
use anyhow::Result;

/// Run the setup script with the new worktree as cwd, inheriting
/// `WORKTREE_PATH`, `WORKTREE_BRANCH`, `REPO_PATH`, `BASE_BRANCH`. Returns
/// whether the script succeeded (or there was nothing to run).
pub fn run_setup(path: &str, branch: &str, base: &str, repo: &str, config: &Config) -> bool {
    let script = config.setup_script();
    if script.is_empty() {
        return true;
    }
    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(path)
        .env("WORKTREE_PATH", path)
        .env("WORKTREE_BRANCH", branch)
        .env("REPO_PATH", repo)
        .env("BASE_BRANCH", util::strip_remote(base))
        .status();
    matches!(status, Ok(s) if s.success())
}

pub fn run_cli(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        anyhow::bail!("usage: setup <path> <branch> <base> <repo>");
    }
    let config = Config::load()?;
    if run_setup(&args[0], &args[1], &args[2], &args[3], &config) {
        Ok(())
    } else {
        anyhow::bail!("setup script failed")
    }
}

/// Detached background mode: run the setup script, then notify Herdr.
pub fn run_background(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        anyhow::bail!("usage: setup-bg <path> <branch> <base> <repo>");
    }
    let (path, branch, base, repo) = (&args[0], &args[1], &args[2], &args[3]);
    let config = Config::load()?;
    let ok = run_setup(path, branch, base, repo, &config);
    let log = std::env::var("WT_LOG").unwrap_or_default();
    if ok {
        crate::herdr::notify(
            "worktree ready",
            &format!("setup complete for '{branch}'"),
            "done",
        );
    } else {
        let body = if log.is_empty() {
            format!("setup failed for '{branch}'")
        } else {
            format!("setup failed for '{branch}' — {log}")
        };
        crate::herdr::notify("worktree setup failed", &body, "request");
    }
    Ok(())
}
