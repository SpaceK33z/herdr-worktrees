//! Non-interactive worktree creation: the same pipeline the popup runs,
//! exposed as `herdr-worktrees create` so agents and scripts can produce a
//! worktree that honors every configured setting instead of raw
//! `git worktree add`.
//!
//! The popup and this command share [`create`]; the popup adds its open-in-
//! pane flow on top, while the CLI prepares the checkout synchronously — an
//! agent needs `.env` copied and the setup script finished before it starts
//! working, not notified about it later.

use crate::config::{apply_branch_prefix, branch_short_name, Config};
use crate::git;
use crate::setup;
use crate::tty;
use crate::util;
use anyhow::{bail, Context as _, Result};

/// What one creation produced, for callers to report or print.
#[derive(Debug)]
pub struct Created {
    pub path: String,
    pub branch: String,
    pub base: String,
    /// The branch already existed and was checked out rather than created.
    pub existed: bool,
}

impl Created {
    /// "created" or "checked out", for one-line reports.
    pub fn action(&self) -> &'static str {
        if self.existed {
            "checked out"
        } else {
            "created"
        }
    }
}

/// Create (or check out) `name` as a worktree of `repo`, honoring everything
/// the popup honors: the branch prefix (unless `exact_branch`), the
/// `worktree-path` template, base resolution (`base` overrides it), fetch-
/// before-create, name validation, and git's own diagnostics on failure.
pub fn create(
    config: &Config,
    repo: &str,
    name: &str,
    base: Option<&str>,
    exact_branch: bool,
) -> Result<Created> {
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let final_branch = if exact_branch {
        name.to_string()
    } else {
        apply_branch_prefix(name, &prefix)
    };
    let short = branch_short_name(&final_branch, &prefix);
    let base = match base {
        Some(base) => base.to_string(),
        None => git::resolve_base_ref(config, repo, &git::current_branch()),
    };
    let path = config.render_worktree_path(&final_branch, &short, &base, repo, &user);

    if !valid_branch_name(repo, &final_branch) {
        bail!("'{final_branch}' is not a valid branch name");
    }

    // Every `worktree add` runs with `-C repo`, so a relative `worktree-path`
    // template resolves against the repo root rather than the caller's cwd.
    let existed = git::ref_exists(repo, &format!("refs/heads/{final_branch}"));
    if existed {
        // branch exists but has no checkout yet -> check it out into a new worktree
        worktree_add(&[
            "-C",
            repo,
            "worktree",
            "add",
            path.as_str(),
            final_branch.as_str(),
        ])?;
    } else {
        // Only a brand-new branch starts from `base`, so only that path needs
        // the base to be current.
        refresh_base(config, repo, &base);
        worktree_add(&[
            "-C",
            repo,
            "worktree",
            "add",
            path.as_str(),
            "-b",
            final_branch.as_str(),
            base.as_str(),
        ])?;
    }

    Ok(Created {
        path,
        branch: final_branch,
        base,
        existed,
    })
}

/// `git worktree add` for an explicit user action: git's own diagnostics reach
/// the output (an existing path, a branch checked out elsewhere, a bad name),
/// so the error only has to say which step they belong to.
pub(crate) fn worktree_add(args: &[&str]) -> Result<()> {
    if !git::git_inherit(args) {
        bail!("git worktree add failed — see the error above");
    }
    Ok(())
}

/// Reject a name `git` would refuse before `git worktree add` fails on it.
pub fn valid_branch_name(repo: &str, name: &str) -> bool {
    git::git_success(&["-C", repo, "check-ref-format", "--branch", name])
}

/// Refresh the base's remote-tracking ref so the new branch starts from the
/// current upstream tip. Purely advisory: an unreachable remote, a rejected
/// fetch, or one that outruns [`git::FETCH_TIMEOUT`] leaves the local copy in
/// place and creation continues from it.
fn refresh_base(config: &Config, repo: &str, base: &str) {
    if !config.fetch_before_create() || git::remote_base_parts(repo, base).is_none() {
        return;
    }
    let progress = tty::spinner(format!("Fetching {base}…"));
    let outcome = git::fetch_base(repo, base);
    progress.finish_and_clear();
    if outcome == git::FetchBase::Failed {
        tty::warn(&format!(
            "could not fetch {base} — creating from the local copy"
        ));
    }
}

const USAGE: &str = concat!(
    "usage: herdr-worktrees create <branch> [--base <ref>] [--exact] [--json] [--no-setup]\n",
    "\n",
    "Creates a worktree with the plugin's settings applied: branch prefix,\n",
    "worktree-path template, base resolution, fetch-before-create,\n",
    ".worktreeinclude copies, and the setup script. Prints the worktree path.\n",
);

/// The `create` entry point: one argument naming the branch, plus flags.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut name: Option<String> = None;
    let mut base: Option<String> = None;
    let mut exact = false;
    let mut json = false;
    let mut no_setup = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                i += 1;
                base = Some(
                    args.get(i)
                        .cloned()
                        .filter(|v| !v.is_empty())
                        .with_context(|| "--base requires a value")?,
                );
            }
            "--exact" => exact = true,
            "--json" => json = true,
            "--no-setup" => no_setup = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if other.starts_with('-') && other != "-" => {
                bail!("unknown flag '{other}'\n{USAGE}");
            }
            other => {
                if name.is_some() {
                    bail!("expected exactly one branch name, got '{other}' too\n{USAGE}");
                }
                name = Some(other.to_string());
            }
        }
        i += 1;
    }
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        bail!("a branch name is required\n{USAGE}");
    };

    let config = Config::load()?;
    let repo = git::repo_root()?.to_string_lossy().into_owned();
    let created = create(&config, &repo, &name, base.as_deref(), exact)?;

    let setup_summary = if no_setup || !setup::has_work(&repo, &config) {
        None
    } else {
        let prepared = setup::run_setup(
            &created.path,
            &created.branch,
            &created.base,
            &repo,
            &config,
        );
        if !prepared.ok() {
            bail!(
                "{} — worktree left in place at {}",
                prepared.summary(),
                created.path
            );
        }
        Some(prepared.summary())
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "path": created.path,
                "branch": created.branch,
                "base": util::strip_remote(&created.base),
                "action": created.action(),
                "setup": setup_summary,
            })
        );
    } else {
        println!(
            "{} {} from {} at {}",
            created.action(),
            created.branch,
            util::strip_remote(&created.base),
            created.path
        );
        if let Some(summary) = setup_summary {
            println!("{summary}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_rejects_missing_and_extra_names() {
        let error = run_cli(&[]).unwrap_err().to_string();
        assert!(error.contains("branch name is required"), "{error}");

        let error = run_cli(&args(&["a", "b"])).unwrap_err().to_string();
        assert!(error.contains("exactly one branch name"), "{error}");

        let error = run_cli(&args(&["--base"])).unwrap_err().to_string();
        assert!(error.contains("--base requires a value"), "{error}");

        let error = run_cli(&args(&["--bogus"])).unwrap_err().to_string();
        assert!(error.contains("unknown flag '--bogus'"), "{error}");
    }

    #[test]
    fn cli_prints_usage_on_help() {
        assert!(run_cli(&args(&["--help"])).is_ok());
    }

    /// Build a config from TOML: `Config` carries private memoization state,
    /// so the file format is the only way to construct one from outside.
    fn config_from(toml: &str) -> Config {
        toml::from_str(toml).expect("test config to parse")
    }

    fn scratch_repo(tag: &str) -> (String, String) {
        let dir = std::env::temp_dir().join(format!(
            "herdr-wt-create-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let repo = dir.join("repo").to_string_lossy().into_owned();
        assert!(git::git_success(&[
            "-C", &repo, "init", "--quiet", "-b", "main"
        ]));
        git::git_stdout(&[
            "-C",
            &repo,
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "init",
        ]);
        let parent = dir.to_string_lossy().into_owned();
        (parent, repo)
    }

    #[test]
    fn create_applies_prefix_and_template_and_records_the_base() {
        let (parent, repo) = scratch_repo("prefix");
        let config = config_from(
            r#"
branch-prefix = "kees/"
worktree-path = "{{ repo_path }}/../wt-{{ branch_short }}"
fetch-before-create = false
"#,
        );
        let created = create(&config, &repo, "parser-fix", None, false).unwrap();
        assert_eq!(created.branch, "kees/parser-fix");
        assert_eq!(created.base, "main");
        assert!(!created.existed);
        // The rendered path is normalized, so `..` never appears in it.
        assert_eq!(created.path, format!("{parent}/wt-parser-fix"));
        assert!(git::ref_exists(&repo, "refs/heads/kees/parser-fix"));
        let listed = git::git_stdout(&["-C", &repo, "worktree", "list", "--porcelain"]);
        assert!(listed.contains("wt-parser-fix"), "{listed}");
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_created() {
        let (_parent, repo) = scratch_repo("existing");
        git::git_stdout(&["-C", &repo, "branch", "topic"]);
        let config = config_from("fetch-before-create = false\n");
        let created = create(&config, &repo, "topic", None, true).unwrap();
        assert!(created.existed);
        assert_eq!(created.branch, "topic");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn an_invalid_name_is_rejected_before_git_runs() {
        let (_parent, repo) = scratch_repo("invalid");
        let config = config_from("");
        let error = create(&config, &repo, "bad name", None, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a valid branch name"), "{error}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }
}
