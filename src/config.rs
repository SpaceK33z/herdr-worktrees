//! Plugin config: discovery, TOML parsing, accessors, and path templating.

use crate::util;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "worktree-path")]
    pub worktree_path: Option<String>,
    #[serde(rename = "base-branch")]
    pub base_branch: Option<String>,
    #[serde(rename = "branch-prefix")]
    pub branch_prefix: Option<String>,
    #[serde(rename = "open-mode")]
    pub open_mode: Option<String>,
    #[serde(rename = "github-prs")]
    pub github_prs: Option<bool>,
    #[serde(rename = "show-worktree-name")]
    pub show_worktree_name: Option<bool>,
    #[serde(rename = "pr-checkout")]
    pub pr_checkout: Option<bool>,
    pub popup: Popup,
    pub remove: Remove,
    #[serde(rename = "pre-start")]
    pub pre_start: PreStart,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Popup {
    pub width: Option<String>,
    pub height: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Remove {
    #[serde(rename = "delete-branch")]
    pub delete_branch: Option<bool>,
    pub force: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreStart {
    #[serde(rename = "setup-worktree")]
    pub setup_worktree: Option<String>,
}

impl Config {
    /// Load the plugin config from `HERDR_PLUGIN_CONFIG_DIR` (or the herdr
    /// plugin config dir, or a sane fallback). Missing files mean all defaults.
    pub fn load() -> Result<Config> {
        let file = config_file_path();
        if file.exists() {
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let cfg: Config =
                toml::from_str(&raw).with_context(|| format!("parsing {}", file.display()))?;
            return Ok(cfg);
        }
        Ok(Config::default())
    }

    pub fn worktree_path_template(&self) -> String {
        self.worktree_path
            .clone()
            .unwrap_or_else(|| "{{ repo_path }}/.worktrees/{{ branch | sanitize }}".to_string())
    }

    /// "tab" or "workspace" (default).
    pub fn open_mode(&self) -> &str {
        match self.open_mode.as_deref() {
            Some("tab") => "tab",
            _ => "workspace",
        }
    }

    pub fn github_prs(&self) -> bool {
        self.github_prs.unwrap_or(false)
    }

    pub fn show_worktree_name(&self) -> bool {
        self.show_worktree_name.unwrap_or(true)
    }

    pub fn pr_checkout(&self) -> bool {
        self.pr_checkout.unwrap_or(true)
    }

    pub fn delete_branch(&self) -> bool {
        self.remove.delete_branch.unwrap_or(false)
    }

    pub fn force(&self) -> bool {
        self.remove.force.unwrap_or(false)
    }

    /// (width, height) from `[popup]`, defaulting to 90% × 70%.
    pub fn popup(&self) -> (String, String) {
        (
            self.popup
                .width
                .clone()
                .unwrap_or_else(|| "90%".to_string()),
            self.popup
                .height
                .clone()
                .unwrap_or_else(|| "70%".to_string()),
        )
    }

    pub fn setup_script(&self) -> String {
        self.pre_start.setup_worktree.clone().unwrap_or_default()
    }

    /// The branch prefix after `{{ user }}` (and friends) are expanded.
    pub fn resolved_prefix(&self, user: &str) -> String {
        let tpl = self.branch_prefix.clone().unwrap_or_default();
        util::render(&tpl, &[("user", user)])
    }

    /// Expand the `worktree-path` template for a concrete branch.
    pub fn render_worktree_path(
        &self,
        branch: &str,
        branch_short: &str,
        base: &str,
        repo_path: &str,
        user: &str,
    ) -> String {
        let repo_name = Path::new(repo_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        util::render(
            &self.worktree_path_template(),
            &[
                ("repo_path", repo_path),
                ("repo_name", &repo_name),
                ("branch", branch),
                ("branch_short", branch_short),
                ("base", &util::strip_remote(base)),
                ("user", user),
            ],
        )
    }
}

/// Strip the configured prefix from a branch name.
pub fn branch_short_name(branch: &str, prefix: &str) -> String {
    if !prefix.is_empty() && branch.starts_with(prefix) {
        branch[prefix.len()..].to_string()
    } else {
        branch.to_string()
    }
}

/// Prepend the prefix, honoring a leading `/` opt-out and avoiding double-prefixing.
pub fn apply_branch_prefix(name: &str, prefix: &str) -> String {
    if let Some(stripped) = name.strip_prefix('/') {
        return stripped.to_string();
    }
    if !prefix.is_empty() && !name.starts_with(prefix) {
        format!("{prefix}{name}")
    } else {
        name.to_string()
    }
}

fn config_file_path() -> PathBuf {
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("config.toml");
        }
    }
    let plugin_id = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "worktrees".to_string());
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    if let Ok(out) = std::process::Command::new(&herdr)
        .args(["plugin", "config-dir", &plugin_id])
        .output()
    {
        if out.status.success() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !dir.is_empty() {
                return PathBuf::from(dir).join("config.toml");
            }
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .ok()
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("herdr/plugins/config/worktrees/config.toml")
}
