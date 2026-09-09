//! Bring the base branch into a worktree, and hand the conflicts to an agent.
//!
//! The point of this path is that the ordinary case never involves a human:
//! refresh the base, merge or rebase it in with an autostash, and say what
//! happened. Only when git stops on conflicts does an agent get started — in
//! the worktree that holds them, already carrying a prompt describing exactly
//! what is left to finish.

use crate::config::Config;
use crate::git;
use crate::herdr;
use crate::status;
use crate::tty;
use crate::util;
use anyhow::{anyhow, bail, Context as _, Result};

/// The one-line task a conflict agent is started with. It stays one line
/// because `herdr agent prompt` submits the text it sends, so a newline would
/// submit the first line on its own.
pub const DEFAULT_PROMPT: &str = "`{{ base }}` was {{ past }} into `{{ branch }}` in this worktree and git stopped on conflicts in {{ files }}. Resolve every conflict keeping the intent of both sides, stage the resolved files, and run `git {{ continue }}` until it finishes. Never run --abort or --skip, do not push, and change nothing unrelated.";

/// Appended when git had to stash uncommitted work to start: agents otherwise
/// see a stash entry appear and try to restore it by hand, on top of a tree git
/// is about to restore itself.
const STASH_NOTE: &str = " Your uncommitted changes were auto-stashed; git restores them when the {{ strategy }} completes, so leave the stash alone.";

/// How the base branch is brought in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Merge,
    Rebase,
}

impl Strategy {
    /// Parse the `[update] strategy` setting.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "merge" => Some(Self::Merge),
            "rebase" => Some(Self::Rebase),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }

    /// Past tense, for the sentence the agent reads.
    fn past(self) -> &'static str {
        match self {
            Self::Merge => "merged",
            Self::Rebase => "rebased",
        }
    }

    /// The git arguments that bring `base` in. `--autostash` is what keeps a
    /// dirty worktree — the normal state of one an agent is working in — from
    /// blocking the update.
    fn args(self, base: &str) -> Vec<&str> {
        match self {
            Self::Merge => vec!["merge", "--autostash", "--no-edit", base],
            Self::Rebase => vec!["rebase", "--autostash", base],
        }
    }

    /// What resumes the operation once the conflicts are staged.
    fn continue_command(self) -> &'static str {
        match self {
            Self::Merge => "merge --continue",
            Self::Rebase => "rebase --continue",
        }
    }
}

/// What bringing the base in did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The base is already an ancestor of the branch; there was nothing to do.
    AlreadyCurrent,
    Updated,
    /// Git stopped, leaving these paths unmerged in the worktree.
    Conflicted {
        files: Vec<String>,
        /// Whether uncommitted work was stashed to get this far.
        stashed: bool,
        operation: ConflictOperation,
    },
}

/// Actual Git state, not the configured strategy. Autostash restoration has
/// no merge/rebase to continue, and must not pop the retained stash again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictOperation {
    Merge,
    Rebase,
    Restoration,
}

fn conflict_operation(path: &str) -> ConflictOperation {
    let exists = |name: &str| {
        let p = git::git_stdout(&[
            "-C",
            path,
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            name,
        ]);
        std::path::Path::new(p.trim()).exists()
    };
    if exists("rebase-merge") || exists("rebase-apply") {
        ConflictOperation::Rebase
    } else if exists("MERGE_HEAD") {
        ConflictOperation::Merge
    } else {
        ConflictOperation::Restoration
    }
}

fn conflict_prompt(
    template: &str,
    branch: &str,
    base: &str,
    operation: ConflictOperation,
    files: &[String],
    stashed: bool,
) -> String {
    match operation {
        ConflictOperation::Merge => build_prompt(template, branch, base, Strategy::Merge, files, stashed),
        ConflictOperation::Rebase => build_prompt(template, branch, base, Strategy::Rebase, files, stashed),
        ConflictOperation::Restoration => format!("Resolve the unmerged working-tree changes in {} on `{branch}` after updating from `{base}`. No merge or rebase is active: do not run merge/rebase --continue. Keep both sides' intent and stage resolved files; leave the restored work uncommitted. Git may have retained an autostash: do not pop, apply or drop it again; preserve it until the restored work is verified. Do not push or change unrelated files.", describe_files(files)),
    }
}

/// The paths git has left unmerged in `path`.
pub fn conflicted_files(path: &str) -> Vec<String> {
    git::git_stdout(&["-C", path, "diff", "--name-only", "--diff-filter=U"])
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `path` holds committed-or-staged work that `--autostash` would sweep
/// aside. Untracked files are excluded because a plain stash leaves them alone.
fn has_stashable_changes(path: &str) -> bool {
    let Ok(output) = git::git_output(&["-C", path, "status", "--porcelain"]) else {
        return false;
    };
    let counts = status::parse_porcelain(&output.stdout);
    counts.staged > 0 || counts.unstaged > 0
}

/// Bring `base` into the checkout at `path`.
pub fn bring_in(path: &str, base: &str, strategy: Strategy) -> Result<Outcome> {
    // A checkout already stopped mid-merge or mid-rebase has to be finished
    // before anything new can come in — and is exactly the state an agent is
    // wanted for, so report it as the conflict it is instead of failing.
    let pending = conflicted_files(path);
    if !pending.is_empty() {
        return Ok(Outcome::Conflicted {
            files: pending,
            stashed: false,
            operation: conflict_operation(path),
        });
    }
    if git::git_success(&["-C", path, "merge-base", "--is-ancestor", base, "HEAD"]) {
        return Ok(Outcome::AlreadyCurrent);
    }

    let stashed = has_stashable_changes(path);
    let mut args = vec!["-C", path];
    args.extend(strategy.args(base));
    let output =
        git::git_output(&args).with_context(|| format!("running git {}", strategy.as_str()))?;
    let files = conflicted_files(path);
    if !files.is_empty() {
        return Ok(Outcome::Conflicted {
            files,
            stashed,
            operation: conflict_operation(path),
        });
    }
    if output.status.success() {
        return Ok(Outcome::Updated);
    }
    // Not a conflict: git refused for some other reason and left nothing to
    // resolve, so its own message is the only useful thing to pass on.
    let detail = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| "no output".to_string());
    bail!("git {} {base} failed: {detail}", strategy.as_str())
}

/// Fill in the prompt template the agent is started with.
pub fn build_prompt(
    template: &str,
    branch: &str,
    base: &str,
    strategy: Strategy,
    files: &[String],
    stashed: bool,
) -> String {
    let mut template = template.to_string();
    if stashed {
        template.push_str(STASH_NOTE);
    }
    util::render(
        &template,
        &[
            ("branch", branch),
            ("base", base),
            ("strategy", strategy.as_str()),
            ("past", strategy.past()),
            ("continue", strategy.continue_command()),
            ("files", &describe_files(files)),
        ],
    )
}

/// Name the conflicted files without pasting a hundred paths into a prompt: the
/// agent can list the rest itself, and a truncated prompt still points it at the
/// right part of the tree.
fn describe_files(files: &[String]) -> String {
    const SHOWN: usize = 10;
    if files.is_empty() {
        return "the working tree".to_string();
    }
    let named = files
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().saturating_sub(SHOWN) {
        0 => named,
        1 => format!("{named} and 1 more file"),
        rest => format!("{named} and {rest} more files"),
    }
}

/// `ctrl-u`: bring `base` into the worktree at `path`, starting an agent on
/// whatever git could not resolve.
pub fn update_worktree(
    path: &str,
    branch: &str,
    base: &str,
    config: &Config,
    repo: &str,
    dry_run: bool,
) -> Result<Outcome> {
    let raw = config.update_strategy();
    let strategy = Strategy::parse(raw)
        .ok_or_else(|| anyhow!("unknown update strategy '{raw}' (expected merge or rebase)"))?;

    if dry_run {
        println!("{} {base} into {branch} ({path})", strategy.as_str());
        return Ok(Outcome::Updated);
    }

    // Bringing in a base that is itself stale is the failure this whole path
    // exists to remove, so refresh it first — advisory, as everywhere else.
    if git::remote_base_parts(repo, base).is_some() {
        let progress = tty::spinner(format!("Fetching {base}…"));
        let fetched = git::fetch_base(repo, base);
        progress.finish_and_clear();
        if fetched == git::FetchBase::Failed {
            tty::warn(&format!(
                "could not fetch {base} — using the last fetched copy"
            ));
        }
    }

    let progress = tty::spinner(format!("{} {base} into {branch}…", verb(strategy)));
    let outcome = bring_in(path, base, strategy);
    progress.finish_and_clear();
    let outcome = outcome?;

    match &outcome {
        Outcome::AlreadyCurrent => {
            println!("{branch} already has {base}");
            herdr::notify(
                "worktree up to date",
                &format!("{branch} already has {base}"),
                "done",
            );
        }
        Outcome::Updated => {
            println!("{} {base} into {branch}", strategy.past());
            herdr::notify(
                "worktree updated",
                &format!("{} {base} into {branch}", strategy.past()),
                "done",
            );
        }
        Outcome::Conflicted {
            files,
            stashed,
            operation,
        } => {
            let prompt = conflict_prompt(
                config.update_prompt(),
                branch,
                base,
                *operation,
                files,
                *stashed,
            );
            hand_off(path, branch, files, &prompt, config, repo)?;
        }
    }
    Ok(outcome)
}

/// The progressive form, for the spinner.
fn verb(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::Merge => "Merging",
        Strategy::Rebase => "Rebasing",
    }
}

/// Start an agent on the conflict, in the worktree that has it.
fn hand_off(
    path: &str,
    branch: &str,
    files: &[String],
    prompt: &str,
    config: &Config,
    repo: &str,
) -> Result<()> {
    let count = files.len();
    let plural = if count == 1 { "" } else { "s" };
    tty::warn(&format!("{count} conflicted file{plural} in {branch}"));

    let Some(kind) = choose_agent(config) else {
        // Cancelling the prompt is not a failure: the conflict is resolvable by
        // hand, and saying where it is beats saying nothing.
        tty::warn(&format!(
            "no agent started — the conflict is waiting in {path}"
        ));
        return Ok(());
    };

    let pane = agent_pane(path, branch, config, repo)?;
    let name = agent_name(branch);

    let progress = tty::spinner(format!("Starting {kind}…"));
    let started = start_agent(&name, &kind, &pane, config);
    progress.finish_and_clear();
    started.with_context(|| format!("starting {kind} for the conflict in {branch}"))?;

    herdr::run_checked(["agent", "prompt", name.as_str(), prompt])
        .with_context(|| format!("handing the conflict to {kind}"))?;
    herdr::notify(
        "merge conflict",
        &format!("{kind} is resolving {count} file{plural} in {branch}"),
        "request",
    );
    Ok(())
}

fn start_agent(name: &str, kind: &str, pane: &str, config: &Config) -> Result<()> {
    let mut args: Vec<String> = vec![
        "agent".into(),
        "start".into(),
        name.into(),
        "--kind".into(),
        kind.into(),
        "--pane".into(),
        pane.into(),
    ];
    let extra = config.update_agent_args(kind);
    if !extra.is_empty() {
        args.push("--".into());
        args.extend(extra);
    }
    herdr::run_checked(&args).map(|_| ())
}

/// The agent kind to hand the conflict to: the configured one, or a prompt when
/// the config says `ask`. `None` means the prompt was cancelled.
fn choose_agent(config: &Config) -> Option<String> {
    let configured = config.update_agent();
    if configured != "ask" {
        return Some(configured.to_string());
    }
    let kinds = config.update_agents();
    tty::pick(
        &kinds.join("\n"),
        "agent",
        "pick an agent to resolve the conflict · esc to skip",
        "",
    )
}

/// Somewhere to start the agent: a fresh split when the worktree is already
/// open — its existing panes belong to whatever was already running there — or
/// the shell pane of the workspace this opens for it.
fn agent_pane(path: &str, branch: &str, config: &Config, repo: &str) -> Result<String> {
    let Some(workspace) = herdr::worktree_workspace_id(path, repo) else {
        return herdr::open_checkout(config.open_mode(), repo, path, branch)
            .ok_or_else(|| anyhow!("could not open {path} to start an agent in"));
    };
    herdr::run(["workspace", "focus", workspace.as_str()]);
    let host = herdr::first_pane(&workspace)
        .ok_or_else(|| anyhow!("workspace {workspace} has no pane to split"))?;
    herdr::split_pane(&host, path, &[])
        .ok_or_else(|| anyhow!("could not split a pane in workspace {workspace}"))
}

/// A readable agent name, unique per branch — one worktree cannot be in two
/// conflicts at once, so the branch alone identifies the work.
fn agent_name(branch: &str) -> String {
    format!("conflict-{}", util::sanitize(branch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn config_from(toml: &str) -> Config {
        toml::from_str(toml).expect("test config to parse")
    }

    #[test]
    fn strategy_names_round_trip_and_reject_anything_else() {
        assert_eq!(Strategy::parse("merge"), Some(Strategy::Merge));
        assert_eq!(Strategy::parse("rebase"), Some(Strategy::Rebase));
        assert_eq!(Strategy::parse("Merge"), None);
        assert_eq!(Strategy::parse(""), None);
        for strategy in [Strategy::Merge, Strategy::Rebase] {
            assert_eq!(Strategy::parse(strategy.as_str()), Some(strategy));
        }
    }

    #[test]
    fn the_prompt_names_the_branch_the_base_and_the_way_out() {
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let prompt = build_prompt(
            DEFAULT_PROMPT,
            "kees/fix",
            "origin/main",
            Strategy::Rebase,
            &files,
            false,
        );
        assert!(
            prompt.contains("`origin/main` was rebased into `kees/fix`"),
            "{prompt}"
        );
        assert!(prompt.contains("src/a.rs, src/b.rs"), "{prompt}");
        assert!(prompt.contains("git rebase --continue"), "{prompt}");
        // A prompt that carried a newline would submit its first line alone.
        assert!(!prompt.contains('\n'), "{prompt}");
        assert!(!prompt.contains("{{"), "unexpanded variable in {prompt}");
    }

    #[test]
    fn the_stash_note_is_added_only_when_something_was_stashed() {
        let files = vec!["a".to_string()];
        let plain = build_prompt(DEFAULT_PROMPT, "b", "main", Strategy::Merge, &files, false);
        let stashed = build_prompt(DEFAULT_PROMPT, "b", "main", Strategy::Merge, &files, true);
        assert!(!plain.contains("auto-stashed"));
        assert!(stashed.contains("auto-stashed"), "{stashed}");
        assert!(stashed.contains("when the merge completes"), "{stashed}");
        assert!(!stashed.contains("{{"), "unexpanded variable in {stashed}");
    }

    #[test]
    fn a_custom_prompt_template_gets_the_same_variables() {
        let files = vec!["a".to_string()];
        let prompt = build_prompt(
            "fix {{ files }} on {{ branch }} from {{ base }} then git {{ continue }}",
            "topic",
            "main",
            Strategy::Merge,
            &files,
            false,
        );
        assert_eq!(prompt, "fix a on topic from main then git merge --continue");
    }

    #[test]
    fn long_conflict_lists_are_summarized_rather_than_pasted() {
        let files: Vec<String> = (0..12).map(|n| format!("f{n}")).collect();
        let described = describe_files(&files);
        assert!(described.ends_with("and 2 more files"), "{described}");
        assert!(!described.contains("f10"), "{described}");
        assert_eq!(describe_files(&files[..11]), {
            let named: Vec<&str> = files[..10].iter().map(String::as_str).collect();
            format!("{} and 1 more file", named.join(", "))
        });
        assert_eq!(describe_files(&[]), "the working tree");
    }

    #[test]
    fn agent_names_survive_a_slash_in_the_branch() {
        assert_eq!(agent_name("kees/fix-it"), "conflict-kees-fix-it");
    }

    #[test]
    fn the_agent_defaults_to_asking_and_can_be_pinned_per_project() {
        assert_eq!(config_from("").update_agent(), "ask");
        assert_eq!(
            config_from("[update]\nagent = \"codex\"").update_agent(),
            "codex"
        );
        assert_eq!(config_from("").update_strategy(), "merge");
        assert!(config_from("")
            .update_agents()
            .contains(&"claude".to_string()));
        assert_eq!(
            config_from("[update]\nagents = [\"pi\"]").update_agents(),
            vec!["pi".to_string()]
        );

        let mut config = config_from(
            r#"
[update]
strategy = "merge"
agent = "claude"

[projects."app".update]
strategy = "rebase"
"#,
        );
        config.apply_project("/home/dev/app");
        assert_eq!(config.update_strategy(), "rebase");
        // Untouched keys keep the top-level answer.
        assert_eq!(config.update_agent(), "claude");
    }

    #[test]
    fn agent_args_merge_per_kind() {
        let mut config = config_from(
            r#"
[update.agent-args]
claude = ["--dangerously-skip-permissions"]
codex = ["--full-auto"]

[projects."app".update.agent-args]
codex = ["--dangerously-bypass-approvals-and-sandbox"]
"#,
        );
        config.apply_project("/home/dev/app");
        assert_eq!(
            config.update_agent_args("claude"),
            vec!["--dangerously-skip-permissions".to_string()]
        );
        assert_eq!(
            config.update_agent_args("codex"),
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        assert!(config.update_agent_args("pi").is_empty());
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

    /// A repo on `main` with one commit, plus a `topic` branch off it.
    fn repo_with_topic(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-wt-update-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("f"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        git(&root, &["checkout", "-q", "-b", "topic"]);
        root
    }

    #[test]
    fn an_unchanged_branch_reports_that_it_already_has_the_base() {
        let repo = repo_with_topic("current");
        let path = repo.to_string_lossy().into_owned();
        assert_eq!(
            bring_in(&path, "main", Strategy::Merge).unwrap(),
            Outcome::AlreadyCurrent
        );
    }

    #[test]
    fn a_clean_merge_brings_the_base_in_without_stopping() {
        let repo = repo_with_topic("clean");
        let path = repo.to_string_lossy().into_owned();
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("other"), "new\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "other"]);
        git(&repo, &["checkout", "-q", "topic"]);

        assert_eq!(
            bring_in(&path, "main", Strategy::Merge).unwrap(),
            Outcome::Updated
        );
        assert!(repo.join("other").exists());
        assert!(conflicted_files(&path).is_empty());
    }

    #[test]
    fn conflicting_edits_are_reported_as_the_files_left_unmerged() {
        let repo = repo_with_topic("conflict");
        let path = repo.to_string_lossy().into_owned();
        std::fs::write(repo.join("f"), "topic\n").unwrap();
        git(&repo, &["commit", "-qam", "topic"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("f"), "main\n").unwrap();
        git(&repo, &["commit", "-qam", "main"]);
        git(&repo, &["checkout", "-q", "topic"]);

        let outcome = bring_in(&path, "main", Strategy::Merge).unwrap();
        assert_eq!(
            outcome,
            Outcome::Conflicted {
                files: vec!["f".to_string()],
                stashed: false,
                operation: ConflictOperation::Merge,
            }
        );
        // A checkout already stopped on a conflict reports it rather than
        // trying to start a second merge on top.
        assert!(matches!(
            bring_in(&path, "main", Strategy::Merge).unwrap(),
            Outcome::Conflicted { .. }
        ));
    }

    #[test]
    fn uncommitted_work_is_stashed_out_of_the_way_and_reported() {
        let repo = repo_with_topic("autostash");
        let path = repo.to_string_lossy().into_owned();
        std::fs::write(repo.join("f"), "topic\n").unwrap();
        git(&repo, &["commit", "-qam", "topic"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("f"), "main\n").unwrap();
        git(&repo, &["commit", "-qam", "main"]);
        git(&repo, &["checkout", "-q", "topic"]);
        // Work in progress on an unrelated file, as an agent's worktree has.
        std::fs::write(repo.join("wip"), "wip\n").unwrap();
        git(&repo, &["add", "wip"]);

        let outcome = bring_in(&path, "main", Strategy::Merge).unwrap();
        assert_eq!(
            outcome,
            Outcome::Conflicted {
                files: vec!["f".to_string()],
                stashed: true,
                operation: ConflictOperation::Merge,
            }
        );
    }
    #[test]
    fn successful_merge_and_rebase_autostash_conflicts_are_restoration_not_updated() {
        for strategy in [Strategy::Merge, Strategy::Rebase] {
            let repo = repo_with_topic(&format!("restore-{}", strategy.as_str()));
            git(&repo, &["checkout", "-q", "main"]);
            std::fs::write(repo.join("f"), "upstream\n").unwrap();
            git(&repo, &["commit", "-qam", "upstream"]);
            git(&repo, &["checkout", "-q", "topic"]);
            std::fs::write(repo.join("f"), "dirty\n").unwrap();
            let outcome = bring_in(repo.to_str().unwrap(), "main", strategy).unwrap();
            assert_eq!(
                outcome,
                Outcome::Conflicted {
                    files: vec!["f".into()],
                    stashed: true,
                    operation: ConflictOperation::Restoration
                }
            );
            assert!(!git(&repo, &["stash", "list"]).is_empty());
            let prompt = conflict_prompt(
                DEFAULT_PROMPT,
                "topic",
                "main",
                ConflictOperation::Restoration,
                &["f".into()],
                true,
            );
            assert!(!prompt.contains("git merge --continue"));
            assert!(!prompt.contains("git rebase --continue"));
            assert!(prompt.contains("do not pop, apply or drop"));
            std::fs::remove_dir_all(repo).unwrap();
        }
    }

    #[test]
    fn pending_rebase_uses_rebase_recovery_even_when_merge_is_configured() {
        let repo = repo_with_topic("actual-rebase");
        std::fs::write(repo.join("f"), "topic\n").unwrap();
        git(&repo, &["commit", "-qam", "topic"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("f"), "main\n").unwrap();
        git(&repo, &["commit", "-qam", "main"]);
        git(&repo, &["checkout", "-q", "topic"]);
        bring_in(repo.to_str().unwrap(), "main", Strategy::Rebase).unwrap();
        assert!(matches!(
            bring_in(repo.to_str().unwrap(), "main", Strategy::Merge).unwrap(),
            Outcome::Conflicted {
                operation: ConflictOperation::Rebase,
                ..
            }
        ));
        let prompt = conflict_prompt(
            DEFAULT_PROMPT,
            "topic",
            "main",
            ConflictOperation::Rebase,
            &["f".into()],
            false,
        );
        assert!(prompt.contains("git rebase --continue"));
        assert!(!prompt.contains("git merge --continue"));
        std::fs::remove_dir_all(repo).unwrap();
    }
}
