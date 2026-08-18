//! The action entry point: open one of the plugin's popup panes in the
//! workspace where the action fired.

use crate::config::Config;
use crate::herdr;
use anyhow::{bail, Result};

pub fn run(args: &[String]) -> Result<()> {
    let entrypoint = match args.first().map(String::as_str) {
        Some("picker") => "picker",
        Some("picker-base") => "picker-base",
        Some("remover") => "remover",
        Some(other) => bail!("unknown entrypoint: {other}"),
        None => "picker",
    };
    let plugin_id = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "worktrees".to_string());
    let config = Config::load()?;
    let (width, height) = config.popup();

    let mut cargs: Vec<String> = vec![
        "plugin".into(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        plugin_id,
        "--entrypoint".into(),
        entrypoint.into(),
        "--placement".into(),
        "popup".into(),
        "--focus".into(),
    ];
    if let Some(c) = workspace_cwd() {
        cargs.push("--cwd".into());
        cargs.push(c);
    }
    cargs.push("--width".into());
    cargs.push(width);
    cargs.push("--height".into());
    cargs.push(height);

    herdr::run(&cargs);
    Ok(())
}

fn workspace_cwd() -> Option<String> {
    let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    if !ctx.is_empty() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ctx) {
            if let Some(c) = v["workspace_cwd"].as_str().or(v["focused_pane_cwd"].as_str()) {
                if !c.is_empty() {
                    return Some(c.to_string());
                }
            }
        }
    }
    std::env::var("HERDR_ACTIVE_PANE_CWD").ok().filter(|c| !c.is_empty())
}
