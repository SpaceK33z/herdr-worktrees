//! The worktree switch/create popup (`prefix+w`).

use crate::background;
use crate::config::{apply_branch_prefix, branch_short_name, Config};
use crate::git;
use crate::herdr;
use crate::model::{self, Engine};
use crate::remove;
use crate::render;
use crate::setup;
use crate::tty;
use crate::util;
use anyhow::{Context as _, Result};
use std::io::Write;

pub fn run(args: &[String]) -> Result<()> {
    let mut base_mode = false;
    let mut dry_run = false;
    let mut dry_name: Option<String> = None;
    for a in args {
        match a.as_str() {
            "--base" => base_mode = true,
            "--dry-run" => dry_run = true,
            other => dry_name = Some(other.to_string()),
        }
    }

    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    let cur_path = git::current_toplevel();

    if dry_run && dry_name.is_some() {
        return dry_run_name(&config, &repo, dry_name.as_deref().unwrap());
    }

    let mut base_override: Option<String> = None;
    if base_mode {
        let base = git::resolve_base_ref(&config, &repo, &git::current_branch());
        base_override = pick_base(&repo, &base);
        if base_override.is_none() {
            return Ok(());
        }
    }

    loop {
        let engine = model::compute_picker_initial(&repo, &config, &state_dir);
        let Some(fzf_out) = run_fzf(&engine, &cur_path)? else {
            return Ok(());
        };

        // esc / no input at all -> close, layout untouched
        if fzf_out.query.is_empty() && fzf_out.selection.is_none() {
            return Ok(());
        }

        // ctrl-p: open the selected branch's PR in the browser.
        if fzf_out.key == "ctrl-p" {
            if let Some(sel) = &fzf_out.selection {
                let parts: Vec<&str> = sel.split('\t').collect();
                if let Some(branch) = parts.first().filter(|branch| !branch.is_empty()) {
                    let entry_kind = parts.get(2).copied().unwrap_or("");
                    open_pr_in_browser(branch, entry_kind, &repo);
                }
            }
            return Ok(());
        }

        // ctrl-d: delete the selected worktree, then refresh the picker in place.
        if fzf_out.key == "ctrl-d" {
            if let Some(sel) = &fzf_out.selection {
                let parts: Vec<&str> = sel.split('\t').collect();
                if parts.len() >= 2 {
                    let del_branch = parts[0].to_string();
                    let del_path = parts[1].to_string();
                    let entry_kind = parts.get(2).copied().unwrap_or("");
                    let del_kind = parts.get(3).copied().unwrap_or("").to_string();
                    let del_changes = parts.get(4).copied().unwrap_or("").to_string();
                    if entry_kind == "worktree" && !del_path.is_empty() && del_path != repo {
                        let _ = remove::delete_worktree(
                            &del_branch,
                            &del_path,
                            &del_kind,
                            &del_changes,
                            &config,
                            &repo,
                        );
                    }
                }
            }
            continue;
        }

        // ctrl-n: create the typed query even when fzf highlights a fuzzy match.
        if fzf_out.key == "ctrl-n" {
            if fzf_out.query.is_empty() {
                return Ok(());
            }
            let base = base_override.as_deref().unwrap_or(&engine.base);
            create_worktree(&fzf_out.query, base, &config, &repo, dry_run, false);
            return Ok(());
        }

        // alt-enter: always create (off the chosen/current base).
        if fzf_out.key == "alt-enter" {
            if fzf_out.query.is_empty() {
                return Ok(());
            }
            let b = match &base_override {
                Some(b) => b.clone(),
                None => match pick_base(&repo, &engine.base) {
                    Some(b) => b,
                    None => return Ok(()),
                },
            };
            create_worktree(&fzf_out.query, &b, &config, &repo, dry_run, false);
            return Ok(());
        }

        if let Some(sel) = &fzf_out.selection {
            let parts: Vec<&str> = sel.split('\t').collect();
            if parts.len() >= 3 {
                let sel_branch = parts[0].to_string();
                let sel_path = parts[1].to_string();
                match entry_route(parts[2]) {
                    EntryRoute::Worktree => {
                        switch_worktree(&sel_path, &sel_branch, &config, &repo, dry_run);
                        return Ok(());
                    }
                    EntryRoute::LocalBranch => {
                        let base = base_override.as_deref().unwrap_or(&engine.base);
                        create_worktree(&sel_branch, base, &config, &repo, dry_run, true);
                        return Ok(());
                    }
                    EntryRoute::RemoteBranch => {
                        checkout_remote_worktree(&sel_branch, &config, &repo, dry_run);
                        return Ok(());
                    }
                    EntryRoute::Section => continue,
                    EntryRoute::Unknown => return Ok(()),
                }
            }
        } else if !fzf_out.query.is_empty() {
            let base = base_override.as_deref().unwrap_or(&engine.base);
            create_worktree(&fzf_out.query, base, &config, &repo, dry_run, false);
            return Ok(());
        }

        return Ok(());
    }
}

struct FzfOut {
    query: String,
    key: String,
    selection: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum EntryRoute {
    Worktree,
    LocalBranch,
    RemoteBranch,
    Section,
    Unknown,
}

fn entry_route(kind: &str) -> EntryRoute {
    match kind {
        "worktree" => EntryRoute::Worktree,
        "branch" => EntryRoute::LocalBranch,
        "remote" => EntryRoute::RemoteBranch,
        "section" => EntryRoute::Section,
        _ => EntryRoute::Unknown,
    }
}

fn run_fzf(engine: &Engine, cur_path: &str) -> Result<Option<FzfOut>> {
    let colors = crate::theme::ThemeColors::load();
    let list = model::render_fzf_lines_with_colors(engine, false, &colors);
    let header = render::render_picker_header(&colors);
    let footer = "\x1b[2menter\x1b[0m switch/create · \x1b[2mctrl-n\x1b[0m new · \x1b[2malt-enter\x1b[0m base… · \x1b[2mctrl-p\x1b[0m open PR · \x1b[2mctrl-d\x1b[0m delete · \x1b[2mctrl-r\x1b[0m refresh · \x1b[2mesc\x1b[0m close";

    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let refresh_cmd = format!("{} --fzf --no-cache", util::shell_escape(&exe));
    let bind = build_fzf_bind(&list, cur_path, &refresh_cmd);

    let mut child = std::process::Command::new("fzf")
        .args([
            "--print-query",
            "--expect=ctrl-n,alt-enter,ctrl-p,ctrl-d",
            "--delimiter=\t",
            "--with-nth=6",
            "--accept-nth=1,2,3,4,5",
            "--prompt=❯ ",
            "--header",
            &header,
            "--footer",
            footer,
            "--ansi",
            "--reverse",
            "--info=inline",
            "--border=rounded",
            "--bind",
            &bind,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawning fzf")?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(list.as_bytes());
    }
    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let query = lines.next().unwrap_or("").to_string();
    let key = lines.next().unwrap_or("").to_string();
    let selection = lines.next().map(|s| s.to_string());
    Ok(Some(FzfOut {
        query,
        key,
        selection,
    }))
}

fn build_fzf_bind(list: &str, cur_path: &str, refresh_cmd: &str) -> String {
    // Draw the cheap local snapshot first, then atomically replace it with the
    // status/PR-enriched list. reload-sync leaves the initial rows interactive
    // while the expensive worktree scans run in the background.
    let load_action = match model::fzf_line_index(list, cur_path) {
        Some(idx) if !cur_path.is_empty() => {
            format!("load:pos({idx})+unbind(load)+reload-sync({refresh_cmd})")
        }
        _ => format!("load:unbind(load)+reload-sync({refresh_cmd})"),
    };
    // Once the user types, move to the first fuzzy match; otherwise fzf can
    // retain an out-of-range position and report no selection on enter/delete.
    format!("{load_action},change:first,ctrl-r:reload-sync({refresh_cmd})")
}

fn pr_head_name<'a>(branch: &'a str, entry_kind: &str) -> &'a str {
    match entry_route(entry_kind) {
        EntryRoute::RemoteBranch => model::origin_local_branch(branch).unwrap_or(branch),
        _ => branch,
    }
}

fn open_pr_in_browser(branch: &str, entry_kind: &str, repo: &str) {
    let head = pr_head_name(branch, entry_kind);
    let opened = std::process::Command::new("gh")
        .args(["pr", "view", head, "--web"])
        .current_dir(repo)
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        tty::err(&format!("could not open a pull request for '{head}'"));
        tty::wait_key();
    }
}

fn pick_base(repo: &str, base: &str) -> Option<String> {
    let mut branches: Vec<String> = Vec::new();
    for l in git::git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads",
    ])
    .lines()
    {
        if !l.is_empty() {
            branches.push(l.to_string());
        }
    }
    for l in git::git_stdout(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/remotes",
    ])
    .lines()
    {
        if l.is_empty() || l.ends_with("/HEAD") {
            continue;
        }
        branches.push(l.to_string());
    }
    branches.sort();
    branches.dedup();
    if branches.is_empty() {
        tty::err("no branches to pick from");
        return None;
    }
    let list = branches.join("\n");
    let query = util::strip_remote(base);

    let mut child = std::process::Command::new("fzf")
        .args([
            "--ansi",
            "--reverse",
            "--info=inline",
            "--border=rounded",
            "--prompt=base branch ❯ ",
            "--header=pick a base branch · esc to cancel",
            "--query",
            &query,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(list.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if choice.is_empty() {
        None
    } else {
        Some(choice)
    }
}

/// Open the checkout in Herdr and return its root pane id (so the setup pane
/// can be split next to it).
fn open_worktree(path: &str, branch: &str, config: &Config, repo: &str) -> Option<String> {
    if config.open_mode() == "tab" {
        herdr::open_tab_pane(herdr::current_workspace().as_deref(), path, branch)
    } else {
        herdr::open_worktree_pane(herdr::root_workspace(repo).as_deref(), repo, path, branch)
    }
}

fn switch_worktree(path: &str, branch: &str, config: &Config, repo: &str, dry_run: bool) {
    if dry_run {
        println!("switch to {branch} ({path})");
        return;
    }
    match herdr::worktree_workspace_id(path, repo) {
        Some(ws) => herdr::run(&["workspace".into(), "focus".into(), ws]),
        None => {
            let _ = open_worktree(path, branch, config, repo);
        }
    }
}

fn checkout_remote_worktree(remote: &str, config: &Config, repo: &str, dry_run: bool) {
    let Some(local_branch) = model::origin_local_branch(remote) else {
        tty::err(&format!("invalid origin branch '{remote}'"));
        tty::wait_key();
        return;
    };
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let short = branch_short_name(local_branch, &prefix);
    let path = config.render_worktree_path(local_branch, &short, remote, repo, &user);

    if dry_run {
        println!("checkout {remote} as {local_branch} at {path}");
        return;
    }

    // Re-check the local ref at selection time. If it appeared since the picker
    // was rendered, use it rather than trying to replace it.
    if git::ref_exists(repo, &format!("refs/heads/{local_branch}")) {
        create_worktree(local_branch, remote, config, repo, false, true);
        return;
    }
    if !git::ref_exists(repo, &format!("refs/remotes/{remote}")) {
        tty::err(&format!("remote branch '{remote}' no longer exists"));
        tty::wait_key();
        return;
    }
    if !add_remote_tracking_worktree(repo, &path, local_branch, remote) {
        tty::err("git worktree add failed (see above)");
        tty::wait_key();
        return;
    }

    open_and_setup_worktree(&path, local_branch, remote, repo, config);
}

fn add_remote_tracking_worktree(repo: &str, path: &str, local: &str, remote: &str) -> bool {
    git::git_success(&[
        "-C", repo, "worktree", "add", "-b", local, "--track", path, remote,
    ])
}

fn create_worktree(
    name: &str,
    base: &str,
    config: &Config,
    repo: &str,
    dry_run: bool,
    exact_branch: bool,
) {
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let final_branch = if exact_branch {
        name.to_string()
    } else {
        apply_branch_prefix(name, &prefix)
    };
    let short = branch_short_name(&final_branch, &prefix);
    let path = config.render_worktree_path(&final_branch, &short, base, repo, &user);

    if dry_run {
        if git::ref_exists(repo, &format!("refs/heads/{final_branch}")) {
            println!("checkout {final_branch} at {path}");
        } else {
            println!("create {final_branch} from {base} at {path}");
        }
        return;
    }

    if git::ref_exists(repo, &format!("refs/heads/{final_branch}")) {
        // branch exists but has no checkout yet -> check it out into a new worktree
        if !git::git_success(&["worktree", "add", path.as_str(), final_branch.as_str()]) {
            tty::err("git worktree add failed (see above)");
            tty::wait_key();
            return;
        }
    } else if !git::git_success(&[
        "worktree",
        "add",
        path.as_str(),
        "-b",
        final_branch.as_str(),
        base,
    ]) {
        tty::err("git worktree add failed (see above)");
        tty::wait_key();
        return;
    }

    open_and_setup_worktree(&path, &final_branch, base, repo, config);
}

/// Open the checkout right away, then run setup in the existing split-pane or
/// detached fallback flow.
fn open_and_setup_worktree(path: &str, branch: &str, base: &str, repo: &str, config: &Config) {
    let shell_pane = open_worktree(path, branch, config, repo);

    if config.setup_script().is_empty() {
        return;
    }

    let Some(shell_pane) = shell_pane else {
        spawn_setup_detached(path, branch, base, repo, config);
        return;
    };
    let mut envs: Vec<(String, String)> = Vec::new();
    for key in [
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_BIN_PATH",
    ] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                envs.push((key.to_string(), v));
            }
        }
    }
    let Some(setup_pane) = herdr::split_pane(&shell_pane, path, &envs) else {
        spawn_setup_detached(path, branch, base, repo, config);
        return;
    };
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let cmd = format!(
        "{} setup-bg {} {} {} {}; exit",
        util::shell_escape(&exe),
        util::shell_escape(path),
        util::shell_escape(branch),
        util::shell_escape(base),
        util::shell_escape(repo),
    );
    herdr::run_in_pane(&setup_pane, &cmd);
}

/// Fallback when the split-pane path is unavailable: run setup detached.
fn spawn_setup_detached(path: &str, branch: &str, base: &str, repo: &str, config: &Config) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let log = background::log_path("setup");
    let args = [
        "setup-bg".to_string(),
        path.to_string(),
        branch.to_string(),
        base.to_string(),
        repo.to_string(),
    ];
    if background::spawn_detached(&exe, &args, &log).is_err()
        && !setup::run_setup(path, branch, base, repo, config)
    {
        tty::err(&format!(
            "setup script failed — worktree left in place at {path}"
        ));
    }
}

fn dry_run_name(config: &Config, repo: &str, name: &str) -> Result<()> {
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let final_branch = apply_branch_prefix(name, &prefix);
    let short = branch_short_name(&final_branch, &prefix);
    let base = git::resolve_base_ref(config, repo, &git::current_branch());
    let path = config.render_worktree_path(&final_branch, &short, &base, repo, &user);

    if git::ref_exists(repo, &format!("refs/heads/{final_branch}")) {
        match find_worktree_path(repo, &final_branch) {
            Some(w) => println!("switch to {final_branch} ({w})"),
            None => println!("checkout {final_branch} at {path}"),
        }
    } else {
        println!("create {final_branch} from {base} at {path}");
    }
    Ok(())
}

fn find_worktree_path(repo: &str, branch: &str) -> Option<String> {
    let porcelain = git::git_stdout(&["-C", repo, "worktree", "list", "--porcelain"]);
    model::parse_worktree_list(&porcelain)
        .into_iter()
        .find(|r| r.branch == branch)
        .map(|r| r.path)
}

#[cfg(test)]
mod tests {
    use super::{
        add_remote_tracking_worktree, build_fzf_bind, entry_route, pr_head_name, EntryRoute,
    };
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git to run");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn unique_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-wt-remote-checkout-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enrichment_binding_keeps_initial_position_and_all_rows() {
        let list = "main\t/repo\tworktree\nother\t/wt\tworktree\n";
        let bind = build_fzf_bind(list, "/wt", "picker --fzf");
        assert!(bind.starts_with("load:pos(2)+unbind(load)+reload-sync(picker --fzf)"));
        assert!(bind.contains("change:first"));
        assert!(bind.contains("ctrl-r:reload-sync(picker --fzf)"));
        assert!(!bind.contains("no-detached"));
    }

    #[test]
    fn remote_entry_kind_routes_to_tracking_checkout() {
        assert_eq!(entry_route("remote"), EntryRoute::RemoteBranch);
    }

    #[test]
    fn pr_lookup_normalizes_only_remote_entries() {
        assert_eq!(pr_head_name("origin/feature/x", "remote"), "feature/x");
        assert_eq!(
            pr_head_name("origin/feature/x", "branch"),
            "origin/feature/x"
        );
        assert_eq!(
            pr_head_name("origin/feature/x", "worktree"),
            "origin/feature/x"
        );
        assert_eq!(pr_head_name("local-feature", "branch"), "local-feature");
    }

    #[test]
    fn remote_checkout_uses_local_name_and_sets_origin_upstream() {
        let tmp = unique_dir();
        let repo = tmp.join("repo");
        let bare = tmp.join("origin.git");
        let worktree = tmp.join("feature-worktree");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Tester"]);
        std::fs::write(repo.join("file"), "initial\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "initial"]);
        git(&tmp, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "main"]);
        git(&repo, &["branch", "feature"]);
        git(&repo, &["push", "-q", "origin", "feature"]);
        git(&repo, &["branch", "-D", "feature"]);

        assert!(add_remote_tracking_worktree(
            repo.to_str().unwrap(),
            worktree.to_str().unwrap(),
            "feature",
            "origin/feature",
        ));
        assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
        assert_eq!(git(&worktree, &["branch", "--show-current"]), "feature");
        assert_eq!(
            git(
                &repo,
                &[
                    "for-each-ref",
                    "--format=%(upstream:short)",
                    "refs/heads/feature",
                ]
            ),
            "origin/feature"
        );

        std::fs::remove_dir_all(tmp).ok();
    }
}
