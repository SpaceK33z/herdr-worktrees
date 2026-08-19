//! Integration tests for the metadata engine and pure helpers.

use herdr_worktrees::config::{apply_branch_prefix, branch_short_name, Config};
use herdr_worktrees::model;
use herdr_worktrees::render;
use herdr_worktrees::util;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git to run");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn unique_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("herdr-wt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn pure_helpers() {
    assert_eq!(util::sanitize("kees/parser-fix"), "kees-parser-fix");
    assert_eq!(util::sanitize("a b@c"), "a-b-c");
    assert_eq!(util::strip_remote("origin/main"), "main");
    assert_eq!(util::strip_remote("refs/remotes/origin/main"), "main");
    assert_eq!(util::strip_remote("main"), "main");
    assert_eq!(
        util::render("x/{{ branch }}/y", &[("branch", "main")]),
        "x/main/y"
    );
    assert_eq!(
        util::render("x/{{ branch | sanitize }}", &[("branch", "main/y")]),
        "x/main-y"
    );

    assert_eq!(render::relative_age(0), "—");
    assert_eq!(render::relative_age(now()), "now");
    assert_eq!(render::relative_age(now() - 7200), "2h");
    assert_eq!(render::relative_age(now() - 259200), "3d");

    assert_eq!(render::pad("abc", 5), "abc  ");
    assert_eq!(render::pad("abcdef", 3), "abc");
    assert_eq!(render::trunc("abcdef", 3), "abc");
    assert_eq!(render::trunc("ab", 5), "ab");
    assert_eq!(
        render::worktree_name("/tmp/worktrees/parser-fix"),
        "parser-fix"
    );
    assert_eq!(render::worktree_name(""), "—");

    assert_eq!(apply_branch_prefix("foo", "kees/"), "kees/foo");
    assert_eq!(apply_branch_prefix("kees/foo", "kees/"), "kees/foo");
    assert_eq!(apply_branch_prefix("/hotfix", "kees/"), "hotfix");
    assert_eq!(branch_short_name("kees/foo", "kees/"), "foo");
    assert_eq!(branch_short_name("foo", "kees/"), "foo");
    assert_eq!(
        model::origin_local_branch("origin/feature/x"),
        Some("feature/x")
    );
    assert_eq!(model::origin_local_branch("origin/HEAD"), None);
    assert_eq!(model::origin_local_branch("upstream/feature"), None);

    let h = render::render_header();
    assert!(h.contains("branch"));
    assert!(h.contains("worktree"));
    assert!(!render::render_header_with_options(false).contains("worktree"));
    assert!(h.contains("pr"));
    assert!(h.contains("review"));
    assert!(h.contains("conflict"));
    assert!(h.contains("sync"));
}

#[test]
fn remote_origin_local_branch_rejects_option_like_local_names() {
    assert_eq!(model::origin_local_branch("origin/-feature"), None);
}

#[test]
fn base_branch_shows_unpushed_commits_without_attributing_them_to_other_branches() {
    let tmp = unique_dir("unpushed-base");
    let scratch = tmp.join("repo");
    std::fs::create_dir_all(&scratch).unwrap();
    git(&scratch, &["init", "-q", "-b", "main"]);
    git(&scratch, &["config", "user.email", "t@t.co"]);
    git(&scratch, &["config", "user.name", "Tester"]);

    std::fs::write(scratch.join("a"), "a\n").unwrap();
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-qm", "initial"]);
    let remote_head = git(&scratch, &["rev-parse", "HEAD"]);
    git(
        &scratch,
        &["update-ref", "refs/remotes/origin/main", remote_head.trim()],
    );
    git(
        &scratch,
        &["branch", "--set-upstream-to=origin/main", "main"],
    );

    for (file, message) in [("b", "local one"), ("c", "local two")] {
        std::fs::write(scratch.join(file), format!("{file}\n")).unwrap();
        git(&scratch, &["add", "."]);
        git(&scratch, &["commit", "-qm", message]);
    }
    git(&scratch, &["branch", "runs"]);
    git(
        &scratch,
        &[
            "worktree",
            "add",
            "-q",
            tmp.join("wt-runs").to_str().unwrap(),
            "runs",
        ],
    );

    let config = Config {
        base_branch: Some("main".to_string()),
        github_prs: Some(false),
        ..Config::default()
    };
    let engine = model::compute_all(
        scratch.to_str().unwrap(),
        &config,
        &tmp.join("state"),
        false,
    );
    let sync = |branch: &str| {
        let wt = engine
            .worktrees
            .iter()
            .find(|wt| wt.branch == branch)
            .unwrap();
        (wt.sync_kind.as_str(), wt.sync.as_str())
    };

    assert_eq!(engine.base, "origin/main");
    assert_eq!(sync("main"), ("ahead", "↑2"));
    assert_eq!(sync("runs"), ("local", "local"));

    let initial =
        model::compute_picker_initial(scratch.to_str().unwrap(), &config, &tmp.join("state"));
    let remove_initial =
        model::compute_remove_initial(scratch.to_str().unwrap(), &config, &tmp.join("state"));
    assert_eq!(remove_initial.worktrees.len(), initial.worktrees.len());
    assert!(remove_initial.branches.is_empty());
    let main = initial
        .worktrees
        .iter()
        .find(|worktree| worktree.branch == "main")
        .unwrap();
    assert_eq!(
        (main.sync_kind.as_str(), main.sync.as_str()),
        ("loading", "…")
    );

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn remote_only_origin_branches_are_emitted_once_and_filtered() {
    let tmp = unique_dir("remote-only");
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t.co"]);
    git(&repo, &["config", "user.name", "Tester"]);
    std::fs::write(repo.join("file"), "initial\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let head = head.trim();

    git(&repo, &["update-ref", "refs/remotes/origin/main", head]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/remote-only", head],
    );
    git(&repo, &["update-ref", "refs/remotes/origin/matched", head]);
    git(&repo, &["branch", "matched", head]);
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/alias",
            "refs/remotes/origin/remote-only",
        ],
    );
    git(
        &repo,
        &["update-ref", "refs/remotes/upstream/not-origin", head],
    );

    let config = Config {
        github_prs: Some(false),
        ..Config::default()
    };
    let engine = model::compute_picker_initial(repo.to_str().unwrap(), &config, &tmp.join("state"));
    let candidates: Vec<_> = engine
        .remote_branches
        .iter()
        .map(|branch| branch.branch.as_str())
        .collect();
    assert_eq!(candidates, vec!["origin/remote-only"]);
    assert_ne!(engine.remote_branches[0].when, "—");

    let rows = model::render_fzf_lines(&engine, false);
    let remote_rows: Vec<_> = rows
        .lines()
        .filter(|line| line.split('\t').nth(2) == Some("remote"))
        .collect();
    assert_eq!(remote_rows.len(), 1);
    assert!(remote_rows[0].starts_with("origin/remote-only\t\tremote\t"));
    assert!(!rows.contains("origin/HEAD"));
    assert!(!rows.contains("origin/matched"));
    assert!(!rows.contains("upstream/not-origin"));
    let local_row = rows.find("matched\t").unwrap();
    let remote_row = rows.find("origin/remote-only\t").unwrap();
    assert!(local_row < remote_row);

    std::fs::remove_dir_all(tmp).ok();
}

#[test]
fn config_and_metadata() {
    let tmp = unique_dir("cfg");
    std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", tmp.join("cfgdir"));
    std::env::set_var("HERDR_PLUGIN_STATE_DIR", tmp.join("state"));
    let cfgdir = tmp.join("cfgdir");
    std::fs::create_dir_all(&cfgdir).unwrap();
    std::fs::write(
        cfgdir.join("config.toml"),
        r#"
worktree-path = "{{ repo_path }}/.wt/{{ branch | sanitize }}"
base-branch = "main"
branch-prefix = "kees/"
open-mode = "workspace"
github-prs = false

[popup]
width = "90%"
height = "70%"

[remove]
delete-branch = true
force = false

[pre-start]
setup-worktree = '''
echo "setting up {{ }}"
'''
"#,
    )
    .unwrap();

    let config = Config::load().unwrap();
    assert_eq!(
        config.worktree_path_template(),
        "{{ repo_path }}/.wt/{{ branch | sanitize }}"
    );
    assert_eq!(config.open_mode(), "workspace");
    assert!(config.show_worktree_name());
    let hidden_names = Config {
        show_worktree_name: Some(false),
        ..Config::default()
    };
    assert!(!hidden_names.show_worktree_name());
    assert!(config.delete_branch());
    assert!(!config.force());
    assert_eq!(config.popup(), ("90%".to_string(), "70%".to_string()));
    assert_eq!(
        config.setup_script().lines().next().unwrap(),
        "echo \"setting up {{ }}\""
    );
    assert_eq!(config.resolved_prefix("tester"), "kees/");
    assert_eq!(config.base_branch.as_deref(), Some("main"));

    // Build a scratch repo with several local branches and worktrees.
    let scratch = tmp.join("repo");
    std::fs::create_dir_all(&scratch).unwrap();
    git(&scratch, &["init", "-q", "-b", "main"]);
    git(&scratch, &["config", "user.email", "t@t.co"]);
    git(&scratch, &["config", "user.name", "Tester"]);
    std::fs::write(scratch.join("a"), "a\n").unwrap();
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-qm", "init"]);

    git(&scratch, &["checkout", "-q", "-b", "merged-branch"]);
    std::fs::write(scratch.join("m"), "m\n").unwrap();
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-qm", "merge me"]);
    git(&scratch, &["checkout", "-q", "main"]);
    git(
        &scratch,
        &[
            "merge",
            "-q",
            "--no-ff",
            "merged-branch",
            "-m",
            "merge branch",
        ],
    );

    git(&scratch, &["checkout", "-q", "-b", "squash-branch"]);
    std::fs::write(scratch.join("s"), "s\n").unwrap();
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-qm", "squash me"]);
    git(&scratch, &["checkout", "-q", "main"]);
    git(&scratch, &["merge", "-q", "--squash", "squash-branch"]);
    git(&scratch, &["commit", "-qm", "squash merge"]);

    git(&scratch, &["checkout", "-q", "-b", "ahead-branch"]);
    std::fs::write(scratch.join("x"), "x\n").unwrap();
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-qm", "ahead work"]);
    git(&scratch, &["checkout", "-q", "main"]);

    git(
        &scratch,
        &[
            "worktree",
            "add",
            "-q",
            tmp.join("wt-merged").to_str().unwrap(),
            "merged-branch",
        ],
    );
    git(
        &scratch,
        &[
            "worktree",
            "add",
            "-q",
            tmp.join("wt-squash").to_str().unwrap(),
            "squash-branch",
        ],
    );
    git(
        &scratch,
        &[
            "worktree",
            "add",
            "-q",
            tmp.join("wt-ahead").to_str().unwrap(),
            "ahead-branch",
        ],
    );
    git(&scratch, &["branch", "empty-pr", "main"]);

    let engine = model::compute_all(
        scratch.to_str().unwrap(),
        &config,
        &tmp.join("state"),
        false,
    );
    let initial =
        model::compute_picker_initial(scratch.to_str().unwrap(), &config, &tmp.join("state"));
    assert!(initial
        .worktrees
        .iter()
        .all(|worktree| worktree.changes == "…"));
    assert_eq!(initial.worktrees.len(), engine.worktrees.len());
    assert_eq!(initial.branches.len(), engine.branches.len());

    let sync_kind = |b: &str| {
        engine
            .worktrees
            .iter()
            .find(|w| w.branch == b)
            .map(|w| w.sync_kind.clone())
            .unwrap_or_default()
    };
    let sync = |b: &str| {
        engine
            .worktrees
            .iter()
            .find(|w| w.branch == b)
            .map(|w| w.sync.clone())
            .unwrap_or_default()
    };

    assert_eq!(sync_kind("main"), "local");
    assert_eq!(sync_kind("merged-branch"), "local");
    assert_eq!(sync_kind("squash-branch"), "local");
    assert_eq!(sync_kind("ahead-branch"), "local");
    assert_eq!(sync("ahead-branch"), "local");
    assert!(
        engine
            .worktrees
            .iter()
            .find(|w| w.branch == "main")
            .unwrap()
            .is_main
    );
    assert_eq!(engine.worktrees.len(), 4);
    assert_eq!(engine.branches.len(), 1);
    assert_eq!(engine.branches[0].branch, "empty-pr");
    assert!(engine.branches[0].path.is_empty());
    assert_eq!(engine.branches[0].changes, "—");

    let picker = model::render_fzf_lines(&engine, false);
    let picker_header = render::render_picker_header();
    assert_eq!(picker_header, render::render_header());
    assert!(!picker.contains("WORKTREES"));
    assert!(!picker.contains("BRANCHES"));
    assert_eq!(picker.lines().count(), 5);
    assert!(picker.contains("wt-ahead"));
    assert!(picker.find("ahead-branch").unwrap() < picker.find("empty-pr").unwrap());

    // Newest worktree first; main checkout last.
    assert_eq!(engine.worktrees[0].branch, "ahead-branch");
    assert_eq!(engine.worktrees[engine.worktrees.len() - 1].branch, "main");

    std::fs::remove_dir_all(&tmp).ok();
}
