//! Integration tests for the metadata engine and pure helpers.

use herdr_worktrees::config::{apply_branch_prefix, branch_short_name, Config};
use herdr_worktrees::model;
use herdr_worktrees::render;
use herdr_worktrees::status;
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

    assert_eq!(apply_branch_prefix("foo", "kees/"), "kees/foo");
    assert_eq!(apply_branch_prefix("kees/foo", "kees/"), "kees/foo");
    assert_eq!(apply_branch_prefix("/hotfix", "kees/"), "hotfix");
    assert_eq!(branch_short_name("kees/foo", "kees/"), "foo");
    assert_eq!(branch_short_name("foo", "kees/"), "foo");

    let h = render::render_header();
    assert!(h.contains("branch"));
    assert!(h.contains("pr"));
    assert!(h.contains("review"));
    assert!(h.contains("conflict"));
    assert!(h.contains("status"));
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
    assert!(config.delete_branch());
    assert!(!config.force());
    assert_eq!(config.popup(), ("90%".to_string(), "70%".to_string()));
    assert_eq!(
        config.setup_script().lines().next().unwrap(),
        "echo \"setting up {{ }}\""
    );
    assert_eq!(config.resolved_prefix("tester"), "kees/");
    assert_eq!(config.base_branch.as_deref(), Some("main"));

    // Build a scratch repo: a merged branch, a squash-merged branch, and an
    // ahead branch, each in its own worktree.
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
    git(&scratch, &["merge", "-q", "--no-ff", "merged-branch", "-m", "merge branch"]);

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

    // A branch equal to the base is locally "merged", but an unmerged PR with
    // no unique commits should display no merged status, even if that local
    // result was already cached.
    let main_head = git(&scratch, &["rev-parse", "main"]);
    git(
        &scratch,
        &["update-ref", "refs/heads/empty-pr", main_head.trim()],
    );
    let empty_head = git(&scratch, &["rev-parse", "empty-pr"]);
    assert!(!empty_head.trim().is_empty());
    assert_eq!(
        status::compute_status(
            "empty-pr",
            "main",
            empty_head.trim(),
            true,
            scratch.to_str().unwrap(),
            &tmp.join("status-state"),
            status::PrStatus::None,
        ),
        ("merged".to_string(), "merged".to_string())
    );
    assert_eq!(
        status::compute_status(
            "empty-pr",
            "main",
            empty_head.trim(),
            true,
            scratch.to_str().unwrap(),
            &tmp.join("status-state"),
            status::PrStatus::Unmerged,
        ),
        ("other".to_string(), "—".to_string())
    );
    git(&scratch, &["branch", "-D", "empty-pr"]);

    git(&scratch, &["worktree", "add", "-q", tmp.join("wt-merged").to_str().unwrap(), "merged-branch"]);
    git(&scratch, &["worktree", "add", "-q", tmp.join("wt-squash").to_str().unwrap(), "squash-branch"]);
    git(&scratch, &["worktree", "add", "-q", tmp.join("wt-ahead").to_str().unwrap(), "ahead-branch"]);
    // The status probe above may use a synthetic commit; restore the inactive
    // branch explicitly before exercising picker discovery.
    git(
        &scratch,
        &["update-ref", "refs/heads/empty-pr", main_head.trim()],
    );

    let engine = model::compute_all(scratch.to_str().unwrap(), &config, &tmp.join("state"), false);

    let status_kind = |b: &str| {
        engine
            .worktrees
            .iter()
            .find(|w| w.branch == b)
            .map(|w| w.status_kind.clone())
            .unwrap_or_default()
    };
    let status = |b: &str| {
        engine
            .worktrees
            .iter()
            .find(|w| w.branch == b)
            .map(|w| w.status.clone())
            .unwrap_or_default()
    };

    assert_eq!(status_kind("main"), "base");
    assert_eq!(status_kind("merged-branch"), "merged");
    assert_eq!(status_kind("squash-branch"), "squashed");
    assert_eq!(status_kind("ahead-branch"), "ahead");
    assert_eq!(status("ahead-branch"), "↑1");
    assert!(engine.worktrees.iter().find(|w| w.branch == "main").unwrap().is_main);
    assert_eq!(engine.worktrees.len(), 4);
    assert_eq!(engine.branches.len(), 1);
    assert_eq!(engine.branches[0].branch, "empty-pr");
    assert!(engine.branches[0].path.is_empty());
    assert_eq!(engine.branches[0].changes, "—");

    let picker = model::render_fzf_lines(&engine, false);
    let worktrees_heading = picker.find("WORKTREES").unwrap();
    let branches_heading = picker.find("BRANCHES").unwrap();
    assert!(worktrees_heading < branches_heading);
    assert!(picker[worktrees_heading..branches_heading].contains("ahead-branch"));
    assert!(picker[branches_heading..].contains("empty-pr"));

    // Newest worktree first; main checkout last.
    assert_eq!(engine.worktrees[0].branch, "ahead-branch");
    assert_eq!(engine.worktrees[engine.worktrees.len() - 1].branch, "main");

    // Cache written for the three checked-out branches and one inactive branch.
    let cache_files: Vec<String> = std::fs::read_dir(tmp.join("state").join("merge-status-v2"))
        .map(|rd| {
            rd.filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(cache_files.len(), 4, "unexpected cache files: {cache_files:?}");

    std::fs::remove_dir_all(&tmp).ok();
}
