//! Prepare a new checkout: carry `.worktreeinclude` files over, then run the
//! configured `[pre-start] setup-worktree` script in it.

use crate::config::Config;
use crate::include;
use crate::util;
use anyhow::Result;

/// What preparing a checkout did, so the caller can report it without
/// re-deriving which half of the work actually ran.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub copy: include::Outcome,
    /// `None` when no setup script is configured.
    pub script_ok: Option<bool>,
}

impl Prepared {
    pub fn ok(&self) -> bool {
        self.copy.failed.is_empty() && self.script_ok.unwrap_or(true)
    }

    /// One line naming what happened, for the Herdr notification: "copied 2
    /// files, setup complete".
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.copy.copied.is_empty() {
            parts.push(format!("copied {}", plural(self.copy.copied.len())));
        }
        if !self.copy.failed.is_empty() {
            parts.push(format!("{} failed to copy", plural(self.copy.failed.len())));
        }
        match self.script_ok {
            Some(true) => parts.push("setup complete".to_string()),
            Some(false) => parts.push("setup script failed".to_string()),
            None => {}
        }
        if parts.is_empty() {
            parts.push("nothing to do".to_string());
        }
        parts.join(", ")
    }
}

fn plural(count: usize) -> String {
    if count == 1 {
        "1 entry".to_string()
    } else {
        format!("{count} entries")
    }
}

/// Copy the repo's `.worktreeinclude` entries into the new checkout, then run
/// the setup script there with `WORKTREE_PATH`, `WORKTREE_BRANCH`, `REPO_PATH`
/// and `BASE_BRANCH` in its environment. Files land before the script runs, so
/// the script can rely on a copied `.env`.
pub fn run_setup(path: &str, branch: &str, base: &str, repo: &str, config: &Config) -> Prepared {
    let copy = include::copy_into(repo, path, config);
    let script = config.setup_script();
    if script.is_empty() {
        return Prepared {
            copy,
            script_ok: None,
        };
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
    Prepared {
        copy,
        script_ok: Some(matches!(status, Ok(s) if s.success())),
    }
}

/// Is there anything to do for a new checkout? Keeps the picker from opening a
/// setup pane for a repo with neither a script nor a `.worktreeinclude`.
pub fn has_work(repo: &str, config: &Config) -> bool {
    !config.setup_script().is_empty() || include::applies(repo, config)
}

pub fn run_cli(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        anyhow::bail!("usage: setup <path> <branch> <base> <repo>");
    }
    let config = Config::load()?;
    let prepared = run_setup(&args[0], &args[1], &args[2], &args[3], &config);
    if prepared.ok() {
        Ok(())
    } else {
        anyhow::bail!("{}", prepared.summary())
    }
}

/// Detached background mode: prepare the checkout, then notify Herdr.
pub fn run_background(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        anyhow::bail!("usage: setup-bg <path> <branch> <base> <repo>");
    }
    let (path, branch, base, repo) = (&args[0], &args[1], &args[2], &args[3]);
    let config = Config::load()?;
    let prepared = run_setup(path, branch, base, repo, &config);
    let log = std::env::var("WT_LOG").unwrap_or_default();
    let summary = prepared.summary();
    if prepared.ok() {
        crate::herdr::notify(
            "worktree ready",
            &format!("{summary} for '{branch}'"),
            "done",
        );
    } else {
        let body = if log.is_empty() {
            format!("{summary} for '{branch}'")
        } else {
            format!("{summary} for '{branch}' — {log}")
        };
        crate::herdr::notify("worktree setup failed", &body, "request");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(copied: usize, failed: usize, script_ok: Option<bool>) -> Prepared {
        Prepared {
            copy: include::Outcome {
                copied: vec!["x".to_string(); copied],
                skipped: Vec::new(),
                failed: vec![("x".to_string(), "boom".to_string()); failed],
            },
            script_ok,
        }
    }

    #[test]
    fn summary_names_both_halves_of_the_work() {
        assert_eq!(
            prepared(2, 0, Some(true)).summary(),
            "copied 2 entries, setup complete"
        );
        assert_eq!(prepared(1, 0, None).summary(), "copied 1 entry");
        assert_eq!(prepared(0, 0, Some(true)).summary(), "setup complete");
        assert_eq!(prepared(0, 0, None).summary(), "nothing to do");
    }

    #[test]
    fn a_failed_copy_fails_the_preparation_even_without_a_script() {
        assert!(prepared(0, 0, Some(true)).ok());
        assert!(prepared(1, 0, None).ok());
        assert!(!prepared(0, 1, None).ok());
        assert_eq!(prepared(0, 1, None).summary(), "1 entry failed to copy");
        assert!(!prepared(2, 0, Some(false)).ok());
    }
}
