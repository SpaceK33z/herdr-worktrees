//! The worktree delete popup (`prefix+d`), and one-off deletes from the picker.

use crate::background;
use crate::config::Config;
use crate::git;
use crate::herdr;
use crate::model;
use crate::render;
use crate::tty;
use anyhow::{Context as _, Result};
use std::io::Write;
use std::path::Path;

pub fn run_interactive() -> Result<()> {
    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    let engine = model::compute_all(&repo, &config, &state_dir, false);

    let header = render::render_header();
    let footer = "enter remove · ctrl-x remove despite dirty/unmerged · esc cancel";

    // Removable = every worktree whose path differs from the main checkout.
    let mut candidates: Vec<String> = Vec::new();
    for wt in &engine.worktrees {
        if wt.path == repo {
            continue;
        }
        let branch_disp = if wt.branch.is_empty() {
            "(detached)".to_string()
        } else {
            wt.branch.clone()
        };
        let row = render::render_row(&branch_disp, wt, &engine.prefix);
        candidates.push(format!(
            "{branch_disp}\t{}\t{}\t{}\t{row}",
            wt.path, wt.status_kind, wt.changes
        ));
    }
    if candidates.is_empty() {
        println!("\x1b[33mNo removable worktrees (only the main checkout exists).\x1b[0m");
        tty::wait_key();
        return Ok(());
    }
    let list = candidates.join("\n");
    let cur_path = git::current_toplevel();

    let Some(sel) = run_remove_fzf(&list, &header, footer, &cur_path)? else {
        return Ok(());
    };

    let parts: Vec<&str> = sel.split('\t').collect();
    let branch = parts.first().copied().unwrap_or("");
    let path = parts.get(1).copied().unwrap_or("");
    let kind = parts.get(2).copied().unwrap_or("");
    let changes = parts.get(3).copied().unwrap_or("");
    delete_worktree(branch, path, kind, changes, &config, &repo)
}

/// The `remove --target <branch> <path> <kind> <changes>` one-off delete used by
/// the picker's ctrl-d.
pub fn run_target(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) != Some("--target") || args.len() != 5 {
        anyhow::bail!("usage: remove --target <branch> <path> <kind> <changes>");
    }
    let branch = &args[1];
    let path = &args[2];
    let kind = &args[3];
    let changes = &args[4];

    let repo_path = git::repo_root()?;
    let repo = repo_path.to_string_lossy().into_owned();
    if path.as_str() == repo.as_str() {
        tty::err("the main checkout can't be removed");
        return Ok(());
    }
    let config = Config::load()?;
    delete_worktree(branch, path, kind, changes, &config, &repo)
}

/// Confirm, remove the checkout, optionally delete the branch, and close the
/// Herdr workspace that was open for it.
pub fn delete_worktree(
    branch: &str,
    path: &str,
    kind: &str,
    _changes: &str,
    config: &Config,
    repo: &str,
) -> Result<()> {
    // The picker list computes a fast tracked-only status; re-check with a full
    // status (including untracked files) so deletion stays safe.
    let dirty = model::worktree_dirty(path);
    let unmerged = matches!(kind, "ahead" | "behind" | "diverged");
    let require_force = (dirty || unmerged) && !config.force();
    let prompt = if require_force {
        format!(
            "  ⚠ '{branch}' has uncommitted changes / unmerged work — ctrl-x to remove anyway, any other key to cancel"
        )
    } else {
        format!("  remove '{branch}'? enter to confirm, any other key to cancel")
    };
    if !tty::confirm(&prompt, require_force) {
        return Ok(());
    }

    // Detach the deletion so the popup closes immediately; a Herdr notification
    // reports the result (including the size) when it finishes.
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let log = background::log_path("remove");
    let args = [
        "remove-bg".to_string(),
        branch.to_string(),
        path.to_string(),
        repo.to_string(),
    ];
    background::spawn_detached(&exe, &args, &log)?;
    println!("removing '{branch}' in the background…");
    Ok(())
}

/// Detached background mode: perform the deletion, then notify Herdr.
pub fn run_background(args: &[String]) -> Result<()> {
    if args.len() != 3 {
        anyhow::bail!("usage: remove-bg <branch> <path> <repo>");
    }
    let (branch, path, repo) = (&args[0], &args[1], &args[2]);
    let config = Config::load()?;
    match perform_delete(branch, path, &config, repo) {
        Ok(bytes) => herdr::notify(
            "worktree removed",
            &format!("removed '{branch}' ({})", human_bytes(bytes)),
            "done",
        ),
        Err(e) => {
            let log = std::env::var("WT_LOG").unwrap_or_default();
            let body = if log.is_empty() {
                format!("failed to remove '{branch}': {e:#}")
            } else {
                format!("failed to remove '{branch}': {e:#} — {log}")
            };
            herdr::notify("worktree removal failed", &body, "request");
        }
    }
    Ok(())
}

/// Delete the checkout's files (counting bytes), then clean up the git
/// worktree registration, branch, and any open Herdr workspace.
fn perform_delete(branch: &str, path: &str, config: &Config, repo: &str) -> Result<u64> {
    // Resolve the open herdr workspace before removing the checkout — once the
    // git worktree is gone, `herdr worktree list` no longer reports its id.
    let wsid = herdr::worktree_workspace_id(path, repo);

    let mut deleted = 0u64;
    delete_tree(Path::new(path), true, &mut deleted);

    if !git::git_success(&["-C", repo, "worktree", "remove", "--force", path]) {
        anyhow::bail!("git worktree remove failed");
    }

    if config.delete_branch() && !branch.is_empty() && branch != "(detached)" {
        let _ = git::git_success(&["-C", repo, "branch", "-D", branch]);
    }

    if let Some(ws) = wsid {
        herdr::run(&["workspace".into(), "close".into(), ws]);
    }

    Ok(deleted)
}

fn run_remove_fzf(list: &str, header: &str, footer: &str, cur_path: &str) -> Result<Option<String>> {
    let mut args: Vec<String> = vec![
        "--delimiter=\t".into(),
        "--with-nth=5".into(),
        "--accept-nth=1,2,3,4".into(),
        "--prompt=remove ❯ ".into(),
        "--header".into(),
        header.to_string(),
        "--footer".into(),
        footer.to_string(),
        "--ansi".into(),
        "--reverse".into(),
        "--info=inline".into(),
        "--border=rounded".into(),
    ];
    if !cur_path.is_empty() {
        if let Some(idx) = model::fzf_line_index(list, cur_path) {
            args.push("--bind".into());
            args.push(format!("load:pos({idx})"));
        }
    }
    let mut child = std::process::Command::new("fzf")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawning fzf")?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(list.as_bytes());
    }
    let out = child.wait_with_output()?;
    let sel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if sel.is_empty() { None } else { Some(sel) })
}

/// Recursively delete a directory tree, accumulating the bytes removed. The
/// top-level `.git` pointer is left for `git worktree remove` to clean up.
fn delete_tree(dir: &Path, skip_git: bool, deleted: &mut u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if skip_git && entry.file_name().to_string_lossy() == ".git" {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            delete_tree(&p, false, deleted);
            let _ = std::fs::remove_dir(&p);
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(&p);
            *deleted += size;
        }
    }
}

/// Human-readable byte size: B, KB, MB, GB.
fn human_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}
