//! Helpers for calling back into Herdr through the CLI.

use anyhow::{bail, Context as _, Result};
use serde_json::Value;
use std::ffi::OsStr;

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// Run a herdr command with inherited stdio; stream output through, ignore failure.
pub fn run<I, S>(args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let _ = std::process::Command::new(herdr_bin()).args(args).status();
}

/// Show a Herdr notification (best-effort; ignored if the session is gone).
pub fn notify(title: &str, body: &str, sound: &str) {
    // Capture (and discard) output so the notification's JSON doesn't leak into
    // a pane that happens to be showing the caller's stdout.
    let _ = std::process::Command::new(herdr_bin())
        .args([
            "notification",
            "show",
            title,
            "--body",
            body,
            "--sound",
            sound,
        ])
        .output();
}

/// Run a herdr command and return its parsed JSON on success.
pub fn json<I, S>(args: I) -> Option<Value>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = std::process::Command::new(herdr_bin())
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()
}

/// Run a herdr command that has to succeed, reporting what it printed when it
/// does not. Used for the commands whose failure the user must hear about —
/// starting an agent and handing it its task — rather than the best-effort
/// calls the rest of this module makes.
pub fn run_checked<I, S>(args: I) -> Result<Value>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let out = std::process::Command::new(herdr_bin())
        .args(&args)
        .output()
        .context("running herdr")?;
    if !out.status.success() {
        let detail = [&out.stderr, &out.stdout]
            .into_iter()
            .map(|stream| String::from_utf8_lossy(stream).trim().to_string())
            .find(|text| !text.is_empty())
            .unwrap_or_else(|| "no output".to_string());
        bail!("{detail}");
    }
    Ok(serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap_or(Value::Null))
}

/// The first pane of a workspace, as somewhere to split from.
pub fn first_pane(workspace: &str) -> Option<String> {
    let panes = json(["pane", "list", "--workspace", workspace])?;
    panes["result"]["panes"].as_array()?.first()?["pane_id"]
        .as_str()
        .map(String::from)
}

/// Split `target` downward (unfocused) and return the new pane id.
pub fn split_pane(target: &str, cwd: &str, envs: &[(String, String)]) -> Option<String> {
    let mut args: Vec<String> = vec![
        "pane".into(),
        "split".into(),
        "--pane".into(),
        target.to_string(),
        "--direction".into(),
        "down".into(),
        "--no-focus".into(),
        "--cwd".into(),
        cwd.to_string(),
    ];
    for (k, v) in envs {
        args.push("--env".into());
        args.push(format!("{k}={v}"));
    }
    json(&args)?["result"]["pane"]["pane_id"]
        .as_str()
        .map(String::from)
}

/// Run a command in a pane (typed into its shell, then Enter).
pub fn run_in_pane(pane: &str, cmd: &str) {
    run(["pane", "run", pane, cmd]);
}

/// Open a worktree checkout as a workspace and return its root pane id.
pub fn open_worktree_pane(
    root_ws: Option<&str>,
    repo: &str,
    path: &str,
    label: &str,
) -> Option<String> {
    let mut args: Vec<String> = vec!["worktree".into(), "open".into()];
    if let Some(ws) = root_ws {
        args.push("--workspace".into());
        args.push(ws.to_string());
    } else {
        args.push("--cwd".into());
        args.push(repo.to_string());
    }
    args.push("--path".into());
    args.push(path.to_string());
    args.push("--label".into());
    args.push(label.to_string());
    args.push("--focus".into());
    json(&args)?["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(String::from)
}

/// Open a checkout as a tab and return its root pane id.
pub fn open_tab_pane(ws: Option<&str>, path: &str, label: &str) -> Option<String> {
    let mut args: Vec<String> = vec!["tab".into(), "create".into()];
    if let Some(ws) = ws {
        args.push("--workspace".into());
        args.push(ws.to_string());
    }
    args.push("--cwd".into());
    args.push(path.to_string());
    args.push("--label".into());
    args.push(label.to_string());
    args.push("--focus".into());
    json(&args)?["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(String::from)
}

/// Open a checkout in the configured `open-mode` and return its shell pane.
pub fn open_checkout(mode: &str, repo: &str, path: &str, label: &str) -> Option<String> {
    if mode == "tab" {
        open_tab_pane(current_workspace().as_deref(), path, label)
    } else {
        open_worktree_pane(root_workspace(repo).as_deref(), repo, path, label)
    }
}

/// The repo's root workspace id (for `herdr worktree open`).
pub fn root_workspace(repo: &str) -> Option<String> {
    json(["worktree", "list", "--cwd", repo])?["result"]["source"]["source_workspace_id"]
        .as_str()
        .map(String::from)
}

/// The open workspace id for a checkout, if any.
pub fn worktree_workspace_id(path: &str, repo: &str) -> Option<String> {
    let v = json(["worktree", "list", "--cwd", repo])?;
    for wt in v["result"]["worktrees"].as_array()? {
        if wt["path"].as_str() == Some(path) {
            if let Some(id) = wt["open_workspace_id"].as_str() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// The workspace the popup was invoked from.
pub fn current_workspace() -> Option<String> {
    let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    if !ctx.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(&ctx) {
            if let Some(ws) = v["workspace_id"]
                .as_str()
                .or(v["focused_workspace_id"].as_str())
            {
                return Some(ws.to_string());
            }
        }
    }
    if let Ok(ws) = std::env::var("HERDR_WORKSPACE_ID") {
        if !ws.is_empty() {
            return Some(ws);
        }
    }
    if let Ok(ws) = std::env::var("HERDR_ACTIVE_WORKSPACE_ID") {
        if !ws.is_empty() {
            return Some(ws);
        }
    }
    json(["pane", "current"])?["result"]["pane"]["workspace_id"]
        .as_str()
        .map(String::from)
}
