//! Plugin config: discovery, TOML parsing, accessors, and path templating.

use crate::detect::{self, Detected};
use crate::util;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const DEFAULT_WORKTREE_PATH: &str = "{{ repo_path }}/.worktrees/{{ branch | sanitize }}";

/// What the `[update] agent = "ask"` prompt offers when the config names no
/// list of its own. Every entry is a Herdr agent kind.
const DEFAULT_UPDATE_AGENTS: [&str; 4] = ["claude", "codex", "opencode", "pi"];

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
    /// Copy the repo's `.worktreeinclude` entries into a new checkout
    /// (default: true; see [`crate::include`]).
    #[serde(rename = "worktree-include")]
    pub worktree_include: Option<bool>,
    /// Refresh the base's remote-tracking ref before creating a branch from it
    /// (default: true).
    #[serde(rename = "fetch-before-create")]
    pub fetch_before_create: Option<bool>,
    /// Fall back to [`crate::detect`] for settings left unset (default: true).
    #[serde(rename = "auto-detect")]
    pub auto_detect: Option<bool>,
    pub popup: Popup,
    pub remove: Remove,
    pub update: Update,
    #[serde(rename = "pre-start")]
    pub pre_start: PreStart,
    /// Per-repo overrides, keyed by `host/owner/repo`, `owner/repo`, the repo
    /// name, or an absolute path. Every key a project table sets wins over the
    /// top-level value.
    pub projects: HashMap<String, Config>,
    /// Memoized detection for one repo; not part of the file format.
    #[serde(skip)]
    detected_path: Arc<OnceLock<(String, Option<Detected>)>>,
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

/// `[update]`: how the base branch is brought into a worktree, and which agent
/// picks up the conflicts git cannot resolve on its own.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Update {
    /// `merge` (default) or `rebase`.
    pub strategy: Option<String>,
    /// A Herdr agent kind (`claude`, `codex`, …), or `ask` (default) to choose
    /// one each time.
    pub agent: Option<String>,
    /// What the `ask` prompt offers.
    pub agents: Option<Vec<String>>,
    /// Extra argv for an agent's executable, keyed by kind.
    #[serde(rename = "agent-args")]
    pub agent_args: HashMap<String, Vec<String>>,
    /// Overrides the one-line task the agent is started with.
    pub prompt: Option<String>,
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
    /// Any `[projects."…"]` table matching the current repo is folded in.
    pub fn load() -> Result<Config> {
        let file = config_file_path();
        let mut config = if file.exists() {
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", file.display()))?
        } else {
            Config::default()
        };
        if !config.projects.is_empty() {
            if let Ok(repo) = crate::git::repo_root() {
                config.apply_project(&repo.to_string_lossy());
            }
        }
        Ok(config)
    }

    /// Overlay the `[projects."…"]` table naming `repo`, preferring the most
    /// specific key when several match.
    pub fn apply_project(&mut self, repo: &str) {
        let ctx = detect::RepoCtx::new(repo);
        let overlay = self
            .projects
            .iter()
            .filter(|(key, _)| ctx.matches(key))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, project)| project.clone());
        if let Some(overlay) = overlay {
            self.merge_from(&overlay);
        }
    }

    /// Take every value `other` sets, leaving the rest untouched.
    fn merge_from(&mut self, other: &Config) {
        fn pick<T: Clone>(base: &mut Option<T>, overlay: &Option<T>) {
            if overlay.is_some() {
                *base = overlay.clone();
            }
        }
        pick(&mut self.worktree_path, &other.worktree_path);
        pick(&mut self.base_branch, &other.base_branch);
        pick(&mut self.branch_prefix, &other.branch_prefix);
        pick(&mut self.open_mode, &other.open_mode);
        pick(&mut self.github_prs, &other.github_prs);
        pick(&mut self.show_worktree_name, &other.show_worktree_name);
        pick(&mut self.pr_checkout, &other.pr_checkout);
        pick(&mut self.worktree_include, &other.worktree_include);
        pick(&mut self.fetch_before_create, &other.fetch_before_create);
        pick(&mut self.auto_detect, &other.auto_detect);
        pick(&mut self.popup.width, &other.popup.width);
        pick(&mut self.popup.height, &other.popup.height);
        pick(&mut self.remove.delete_branch, &other.remove.delete_branch);
        pick(&mut self.remove.force, &other.remove.force);
        pick(&mut self.update.strategy, &other.update.strategy);
        pick(&mut self.update.agent, &other.update.agent);
        pick(&mut self.update.agents, &other.update.agents);
        pick(&mut self.update.prompt, &other.update.prompt);
        // Argv is merged per agent kind, so a project can override how one
        // agent launches without restating the others.
        for (kind, args) in &other.update.agent_args {
            self.update.agent_args.insert(kind.clone(), args.clone());
        }
        pick(
            &mut self.pre_start.setup_worktree,
            &other.pre_start.setup_worktree,
        );
    }

    pub fn auto_detect(&self) -> bool {
        self.auto_detect.unwrap_or(true)
    }

    /// The `worktree-path` template for `repo`: explicit config first, then
    /// whatever the repo's other tooling already declares, then the default.
    pub fn worktree_path_template(&self, repo: &str) -> String {
        if let Some(template) = &self.worktree_path {
            return template.clone();
        }
        if let Some(detected) = self.detected_worktree_path(repo) {
            return detected.value;
        }
        DEFAULT_WORKTREE_PATH.to_string()
    }

    /// Detection result for `repo`, computed at most once per process.
    pub fn detected_worktree_path(&self, repo: &str) -> Option<Detected> {
        if !self.auto_detect() {
            return None;
        }
        if let Some((cached, detected)) = self.detected_path.get() {
            if cached == repo {
                return detected.clone();
            }
        }
        let detected = detect::worktree_path(&detect::RepoCtx::new(repo));
        let _ = self.detected_path.set((repo.to_string(), detected.clone()));
        detected
    }

    /// "tab" or "workspace" (default).
    pub fn open_mode(&self) -> &str {
        match self.open_mode.as_deref() {
            Some("tab") => "tab",
            _ => "workspace",
        }
    }

    pub fn github_prs(&self) -> bool {
        self.github_prs.unwrap_or(true)
    }

    pub fn show_worktree_name(&self) -> bool {
        self.show_worktree_name.unwrap_or(true)
    }

    pub fn pr_checkout(&self) -> bool {
        self.pr_checkout.unwrap_or(true)
    }

    /// Whether creating a branch first refreshes the base's remote-tracking
    /// ref. A failed fetch never blocks creation.
    pub fn fetch_before_create(&self) -> bool {
        self.fetch_before_create.unwrap_or(true)
    }

    pub fn worktree_include(&self) -> bool {
        self.worktree_include.unwrap_or(true)
    }

    /// Delete the branch when its worktree is removed. Defaults to true: a
    /// branch is only auto-deleted when its content is provably on the base
    /// branch (pushed, or landed via squash merge/rebase), and a branch still
    /// checked out in another worktree is never deleted.
    pub fn delete_branch(&self) -> bool {
        self.remove.delete_branch.unwrap_or(true)
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

    /// The raw `[update] strategy`; [`crate::update::Strategy`] rejects a name
    /// it does not know rather than silently picking one.
    pub fn update_strategy(&self) -> &str {
        self.update.strategy.as_deref().unwrap_or("merge")
    }

    /// The agent kind conflicts are handed to, or `ask` to choose each time.
    pub fn update_agent(&self) -> &str {
        self.update
            .agent
            .as_deref()
            .filter(|agent| !agent.is_empty())
            .unwrap_or("ask")
    }

    /// The kinds the `ask` prompt offers.
    pub fn update_agents(&self) -> Vec<String> {
        self.update
            .agents
            .clone()
            .filter(|agents| !agents.is_empty())
            .unwrap_or_else(|| {
                DEFAULT_UPDATE_AGENTS
                    .iter()
                    .map(|a| a.to_string())
                    .collect()
            })
    }

    /// Extra argv for an agent's executable.
    pub fn update_agent_args(&self, kind: &str) -> Vec<String> {
        self.update
            .agent_args
            .get(kind)
            .cloned()
            .unwrap_or_default()
    }

    /// The prompt template the conflict agent starts with.
    pub fn update_prompt(&self) -> &str {
        self.update
            .prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or(crate::update::DEFAULT_PROMPT)
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
        let rendered = util::render(
            &self.worktree_path_template(repo_path),
            &[
                ("repo_path", repo_path),
                ("repo_name", &repo_name),
                ("branch", branch),
                ("branch_short", branch_short),
                ("base", &util::strip_remote(base)),
                ("user", user),
            ],
        );
        // Templates commonly point at a sibling directory ("{{ repo_path }}/../"),
        // but git records the resolved path, so collapse it before we compare.
        util::normalize_path(&rendered)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Config {
        toml::from_str(raw).unwrap()
    }

    #[test]
    fn project_table_overrides_top_level() {
        let mut config = parse(
            r#"
worktree-path = "{{ repo_path }}/.worktrees/{{ branch }}"
base-branch = "main"

[projects."app"]
base-branch = "develop"

[projects."/home/dev/app"]
worktree-path = "/wt/{{ branch }}"

[projects."other"]
worktree-path = "/nope/{{ branch }}"
"#,
        );
        config.apply_project("/home/dev/app");
        // The most specific matching key wins, and leaves the rest alone.
        assert_eq!(config.worktree_path.as_deref(), Some("/wt/{{ branch }}"));
        assert_eq!(config.base_branch.as_deref(), Some("main"));
    }

    #[test]
    fn github_prs_defaults_on_and_can_be_disabled() {
        assert!(parse("").github_prs());
        assert!(!parse("github-prs = false").github_prs());
    }

    #[test]
    fn fetch_before_create_defaults_on_and_is_overridable_per_project() {
        assert!(parse("").fetch_before_create());
        assert!(!parse("fetch-before-create = false").fetch_before_create());

        let mut config = parse(
            r#"
fetch-before-create = false

[projects."app"]
fetch-before-create = true
"#,
        );
        config.apply_project("/home/dev/app");
        assert!(config.fetch_before_create());
    }

    #[test]
    fn nested_tables_merge_per_field() {
        let mut config = parse(
            r#"
[popup]
width = "90%"
height = "70%"

[projects."app".popup]
height = "40%"
"#,
        );
        config.apply_project("/home/dev/app");
        assert_eq!(config.popup(), ("90%".to_string(), "40%".to_string()));
    }

    #[test]
    fn explicit_path_beats_detection() {
        let config = parse(
            r#"
worktree-path = "/wt/{{ branch }}"
"#,
        );
        assert_eq!(
            config.worktree_path_template("/home/dev/app"),
            "/wt/{{ branch }}"
        );
    }

    #[test]
    fn detection_can_be_turned_off() {
        let config = parse("auto-detect = false\n");
        assert!(config.detected_worktree_path("/home/dev/app").is_none());
        assert_eq!(
            config.worktree_path_template("/home/dev/app"),
            DEFAULT_WORKTREE_PATH
        );
    }

    #[test]
    fn sibling_templates_render_to_the_path_git_records() {
        let config = parse(
            r#"
worktree-path = "{{ repo_path }}/../{{ repo_name }}.{{ branch | sanitize }}"
"#,
        );
        assert_eq!(
            config.render_worktree_path("kees/fix", "fix", "main", "/home/dev/app", "kees"),
            "/home/dev/app.kees-fix"
        );
    }
}
