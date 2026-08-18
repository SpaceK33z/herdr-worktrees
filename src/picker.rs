//! The worktree switch/create popup (`prefix+w`).

use crate::background;
use crate::config::{apply_branch_prefix, branch_short_name, Config};
use crate::git;
use crate::herdr;
use crate::model::{self, Engine};
use crate::render;
use crate::remove;
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
        let engine = model::compute_all(&repo, &config, &state_dir, false);
        base_override = pick_base(&repo, &engine.base);
        if base_override.is_none() {
            return Ok(());
        }
    }

    loop {
        let engine = model::compute_all(&repo, &config, &state_dir, false);
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
                if let Some(branch) = sel.split('\t').next().filter(|branch| !branch.is_empty()) {
                    open_pr_in_browser(branch, &repo);
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
                            &del_branch, &del_path, &del_kind, &del_changes, &config, &repo,
                        );
                    }
                }
            }
            continue;
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
                match parts[2] {
                    "worktree" => {
                        switch_worktree(&sel_path, &sel_branch, &config, &repo, dry_run);
                        return Ok(());
                    }
                    "branch" => {
                        let base = base_override.as_deref().unwrap_or(&engine.base);
                        create_worktree(
                            &sel_branch,
                            base,
                            &config,
                            &repo,
                            dry_run,
                            true,
                        );
                        return Ok(());
                    }
                    "section" => continue,
                    _ => return Ok(()),
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

fn run_fzf(engine: &Engine, cur_path: &str) -> Result<Option<FzfOut>> {
    let list = model::render_fzf_lines(engine, false);
    let header = render::render_header();
    let footer = "\x1b[2menter\x1b[0m switch/create · \x1b[2malt-enter\x1b[0m base… · \x1b[2mctrl-p\x1b[0m open PR · \x1b[2mctrl-d\x1b[0m delete · \x1b[2mctrl-r\x1b[0m refresh · \x1b[2mesc\x1b[0m close";

    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let refresh_cmd = format!("{} --fzf --no-detached --no-cache", util::shell_escape(&exe));
    let mut bind = format!("ctrl-r:reload({refresh_cmd})");
    if !cur_path.is_empty() {
        if let Some(idx) = model::fzf_line_index(&list, cur_path) {
            bind = format!("load:pos({idx}),{bind}");
        }
    }

    let mut child = std::process::Command::new("fzf")
        .args([
            "--print-query",
            "--expect=alt-enter,ctrl-p,ctrl-d",
            "--delimiter=\t",
            "--with-nth=6",
            "--accept-nth=1,2,3,4,5",
            "--prompt=worktree ❯ ",
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

fn open_pr_in_browser(branch: &str, repo: &str) {
    let opened = std::process::Command::new("gh")
        .args(["pr", "view", branch, "--web"])
        .current_dir(repo)
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        tty::err(&format!("could not open a pull request for '{branch}'"));
        tty::wait_key();
    }
}

fn pick_base(repo: &str, base: &str) -> Option<String> {
    let mut branches: Vec<String> = Vec::new();
    for l in git::git_stdout(&["-C", repo, "for-each-ref", "--format=%(refname:short)", "refs/heads"]).lines() {
        if !l.is_empty() {
            branches.push(l.to_string());
        }
    }
    for l in git::git_stdout(&["-C", repo, "for-each-ref", "--format=%(refname:short)", "refs/remotes"]).lines() {
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

    // Open the checkout right away so the user can start working, then run the
    // setup script in a split pane that auto-closes when it finishes; a Herdr
    // notification also reports completion.
    let shell_pane = open_worktree(&path, &final_branch, config, repo);

    if config.setup_script().is_empty() {
        return;
    }

    let Some(shell_pane) = shell_pane else {
        spawn_setup_detached(&path, &final_branch, base, repo, config);
        return;
    };
    let mut envs: Vec<(String, String)> = Vec::new();
    for key in ["HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR", "HERDR_BIN_PATH"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                envs.push((key.to_string(), v));
            }
        }
    }
    let Some(setup_pane) = herdr::split_pane(&shell_pane, &path, &envs) else {
        spawn_setup_detached(&path, &final_branch, base, repo, config);
        return;
    };
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let cmd = format!(
        "{} setup-bg {} {} {} {}; exit",
        util::shell_escape(&exe),
        util::shell_escape(&path),
        util::shell_escape(&final_branch),
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
        tty::err(&format!("setup script failed — worktree left in place at {path}"));
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
