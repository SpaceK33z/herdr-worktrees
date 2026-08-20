//! Auto-detect settings other tools (or the repo itself) already declare.
//!
//! Users who run Worktrunk, gwq, phantom or ccmanager have already told *those*
//! tools where worktrees belong, and a repo that keeps worktrees in a fixed spot
//! shows it in `git worktree list`. Rather than making everyone restate that in
//! this plugin's config, every [`Source`] below translates one such convention
//! into a `worktree-path` template written in this plugin's own syntax.
//!
//! Explicit config always wins; detection only fills in what the user left unset.
//!
//! Extending this module:
//! - **another tool**: implement [`Source`] and add it to [`sources`].
//! - **another setting**: add a method to [`Source`] that defaults to `None`,
//!   then a resolver next to [`worktree_path`] that walks the same list.

use crate::util;
use std::path::{Path, PathBuf};

/// How specific a detected value is to this repo. Ranked: something written
/// about *this* repo beats what the repo happens to look like, which in turn
/// beats a tool's global default — that default was never about this repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Configured for this repo specifically.
    Repo,
    /// Inferred from the worktrees this repo already has.
    Observed,
    /// A tool's global setting, which applies to every repo.
    Global,
    /// A weaker signal, used only when nothing else has an opinion.
    Hint,
}

/// A detected value plus where it came from (shown by `herdr-worktrees detect`).
#[derive(Debug, Clone)]
pub struct Detected {
    pub value: String,
    pub source: &'static str,
    pub scope: Scope,
    pub detail: String,
}

impl Detected {
    fn new(
        source: &'static str,
        scope: Scope,
        detail: impl Into<String>,
        value: impl Into<String>,
    ) -> Detected {
        Detected {
            value: value.into(),
            source,
            scope,
            detail: detail.into(),
        }
    }
}

/// The repo facts every source needs: paths plus the remote's host/owner/name,
/// which is how per-project tables in other tools are keyed.
#[derive(Debug, Clone, Default)]
pub struct RepoCtx {
    pub repo: String,
    pub repo_name: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    /// Every spelling a per-project config key may use for this repo.
    pub keys: Vec<String>,
}

impl RepoCtx {
    pub fn new(repo: &str) -> RepoCtx {
        let repo = repo.trim_end_matches('/').to_string();
        let repo_name = Path::new(&repo)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let url = crate::git::git_stdout(&["-C", &repo, "remote", "get-url", "origin"]);
        let (host, owner, name) = parse_remote(url.trim());

        let mut keys = Vec::new();
        if !host.is_empty() && !owner.is_empty() && !name.is_empty() {
            keys.push(format!("{host}/{owner}/{name}"));
        }
        if !owner.is_empty() && !name.is_empty() {
            keys.push(format!("{owner}/{name}"));
        }
        if !name.is_empty() {
            keys.push(name.clone());
        }
        keys.push(repo.clone());
        if !repo_name.is_empty() && !keys.contains(&repo_name) {
            keys.push(repo_name.clone());
        }
        RepoCtx {
            repo,
            repo_name,
            host,
            owner,
            name,
            keys,
        }
    }

    /// Does `key` (from someone else's `[projects."…"]` table) name this repo?
    pub fn matches(&self, key: &str) -> bool {
        let key = normalize_key(key);
        self.keys.iter().any(|k| normalize_key(k) == key)
    }
}

/// One convention for declaring where worktrees live.
///
/// Every method defaults to "no opinion" so adding a setting never breaks the
/// existing sources.
pub trait Source {
    fn id(&self) -> &'static str;

    /// A `worktree-path` template, in this plugin's template syntax.
    fn worktree_path(&self, _ctx: &RepoCtx) -> Option<Detected> {
        None
    }
}

/// Every source, most authoritative first: a tool's config for *this* repo beats
/// its global config, explicit configuration beats what the repo happens to look
/// like, and observed worktrees beat a mere `.gitignore` hint.
pub fn sources() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(Worktrunk),
        Box::new(Gwq),
        Box::new(Phantom),
        Box::new(Ccmanager),
        Box::new(Observed),
        Box::new(IgnoreHint),
    ]
}

/// Where worktrees live: the most repo-specific answer any source can give.
pub fn worktree_path(ctx: &RepoCtx) -> Option<Detected> {
    sources()
        .iter()
        .filter_map(|s| s.worktree_path(ctx))
        // A template that does not vary per branch would collide; ignore it.
        .filter(|d| d.value.contains("{{ branch"))
        .min_by_key(|d| d.scope)
}

/// Every source's verdict, in resolution order, for `herdr-worktrees detect`.
pub fn explain(ctx: &RepoCtx) -> Vec<(&'static str, Option<Detected>)> {
    let mut verdicts: Vec<(&'static str, Option<Detected>)> = sources()
        .iter()
        .map(|s| (s.id(), s.worktree_path(ctx)))
        .collect();
    verdicts.sort_by_key(|(_, d)| d.as_ref().map(|d| d.scope).unwrap_or(Scope::Hint));
    verdicts
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Worktrunk (`wt`): `worktree-path` templates, which use the same `{{ var }}`
/// and `{{ var | sanitize }}` spelling this plugin does.
struct Worktrunk;

impl Source for Worktrunk {
    fn id(&self) -> &'static str {
        "worktrunk"
    }

    fn worktree_path(&self, ctx: &RepoCtx) -> Option<Detected> {
        if let Some(raw) = env_var("WORKTRUNK_WORKTREE_PATH") {
            if let Some(t) = worktrunk_template(&raw, ctx) {
                return Some(Detected::new(
                    self.id(),
                    Scope::Repo,
                    "WORKTRUNK_WORKTREE_PATH",
                    t,
                ));
            }
        }

        // Checked-in project config.
        let project = env_var("WORKTRUNK_PROJECT_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&ctx.repo).join(".config/wt.toml"));
        if let Some(raw) = read_toml(&project)
            .as_ref()
            .and_then(|v| str_at(v, "worktree-path"))
        {
            if let Some(t) = worktrunk_template(&raw, ctx) {
                return Some(Detected::new(self.id(), Scope::Repo, display(&project), t));
            }
        }

        let user = config_home().join("worktrunk/config.toml");
        let value = read_toml(&user)?;
        if let Some((key, table)) = project_table(&value, ctx) {
            if let Some(raw) = str_at(table, "worktree-path") {
                if let Some(t) = worktrunk_template(&raw, ctx) {
                    let detail = format!("{} [projects.\"{key}\"]", display(&user));
                    return Some(Detected::new(self.id(), Scope::Repo, detail, t));
                }
            }
        }
        let raw = str_at(&value, "worktree-path")?;
        let t = worktrunk_template(&raw, ctx)?;
        Some(Detected::new(self.id(), Scope::Global, display(&user), t))
    }
}

/// gwq: a base directory plus a Go-template naming scheme.
struct Gwq;

const GWQ_DEFAULT_TEMPLATE: &str = "{{.Host}}/{{.Owner}}/{{.Repository}}/{{.Branch}}";
const GWQ_DEFAULT_BASEDIR: &str = "~/worktrees";

impl Source for Gwq {
    fn id(&self) -> &'static str {
        "gwq"
    }

    fn worktree_path(&self, ctx: &RepoCtx) -> Option<Detected> {
        let files = [
            (Path::new(&ctx.repo).join(".gwq.toml"), true),
            (config_home().join("gwq/config.toml"), false),
        ];
        for (file, local) in files {
            let Some(value) = read_toml(&file) else {
                continue;
            };
            // A per-repository basedir replaces the whole naming scheme; only
            // the branch is left to distinguish worktrees under it.
            if let Some(entries) = value.get("repository_settings").and_then(|v| v.as_array()) {
                for entry in entries {
                    let matches = entry
                        .get("repository")
                        .and_then(|v| v.as_str())
                        .map(|r| ctx.matches(r))
                        .unwrap_or(false);
                    if !matches {
                        continue;
                    }
                    if let Some(base) = entry.get("basedir").and_then(|v| v.as_str()) {
                        let detail = format!("{} [[repository_settings]]", display(&file));
                        let value = join(&expand_tilde(base), "{{ branch | sanitize }}");
                        return Some(Detected::new(self.id(), Scope::Repo, detail, value));
                    }
                }
            }
            let base = value
                .get("worktree")
                .and_then(|v| v.get("basedir"))
                .and_then(|v| v.as_str())
                .unwrap_or(GWQ_DEFAULT_BASEDIR);
            let naming = value
                .get("naming")
                .and_then(|v| v.get("template"))
                .and_then(|v| v.as_str())
                .unwrap_or(GWQ_DEFAULT_TEMPLATE);
            if let Some(tail) = gwq_template(naming, ctx) {
                let value = join(&expand_tilde(base), &tail);
                // A checked-in `.gwq.toml` is about this repo; the user config is not.
                let scope = if local { Scope::Repo } else { Scope::Global };
                return Some(Detected::new(self.id(), scope, display(&file), value));
            }
        }
        None
    }
}

/// phantom: `worktreesDirectory`, stored either in git config or in
/// `phantom.config.json` at the repo root.
struct Phantom;

impl Source for Phantom {
    fn id(&self) -> &'static str {
        "phantom"
    }

    fn worktree_path(&self, ctx: &RepoCtx) -> Option<Detected> {
        let file = Path::new(&ctx.repo).join("phantom.config.json");
        let json = read_json(&file);

        // phantom joins repo, branch and suffixes with this; anything other than
        // `-` is something `sanitize` cannot reproduce, so leave it alone.
        let separator = git_config(&ctx.repo, "phantom.directoryNameSeparator").or_else(|| {
            json.as_ref()
                .and_then(|v| v.get("directoryNameSeparator"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
        if !matches!(separator.as_deref(), None | Some("-")) {
            return None;
        }

        // A repo-local setting wins over the global preference `phantom
        // preferences set` writes; a config file with no directory means the
        // repo uses phantom's default location.
        let json_dir = json
            .as_ref()
            .and_then(|v| v.get("worktreesDirectory"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let (dir, scope, detail) =
            if let Some(dir) = git_config_local(&ctx.repo, "phantom.worktreesDirectory") {
                (dir, Scope::Repo, "git config --local".to_string())
            } else if let Some(dir) = json_dir {
                (dir, Scope::Repo, display(&file))
            } else if let Some(dir) = git_config(&ctx.repo, "phantom.worktreesDirectory") {
                (dir, Scope::Global, "git config".to_string())
            } else if json.is_some() {
                (
                    ".git/phantom/worktrees".to_string(),
                    Scope::Repo,
                    format!("{} (default location)", display(&file)),
                )
            } else {
                return None;
            };
        let dir = expand_tilde(&dir);
        let dir = if is_absolute(&dir) {
            dir
        } else {
            join("{{ repo_path }}", &dir)
        };
        Some(Detected::new(
            self.id(),
            scope,
            detail,
            join(&dir, "{{ branch | sanitize }}"),
        ))
    }
}

/// ccmanager: a directory pattern containing `{branch}`. Its config keys have
/// moved around between releases, so look for the pattern rather than a key.
struct Ccmanager;

impl Source for Ccmanager {
    fn id(&self) -> &'static str {
        "ccmanager"
    }

    fn worktree_path(&self, _ctx: &RepoCtx) -> Option<Detected> {
        let file = config_home().join("ccmanager/config.json");
        let json = read_json(&file)?;
        let pattern = find_branch_pattern(&json, 0)?;
        let value = pattern.replace("{branch}", "{{ branch | sanitize }}");
        if value.contains('{') && !value.contains("{{") {
            return None; // some other placeholder we cannot render
        }
        let value = expand_tilde(&value);
        let value = if is_absolute(&value) {
            value
        } else {
            join("{{ repo_path }}", &value)
        };
        Some(Detected::new(
            self.id(),
            Scope::Global,
            display(&file),
            value,
        ))
    }
}

/// The repo itself: infer the layout from the worktrees that already exist.
/// This is the tool-agnostic fallback — it works for hand-rolled scripts too.
struct Observed;

impl Source for Observed {
    fn id(&self) -> &'static str {
        "observed"
    }

    fn worktree_path(&self, ctx: &RepoCtx) -> Option<Detected> {
        let porcelain =
            crate::git::git_stdout(&["-C", &ctx.repo, "worktree", "list", "--porcelain"]);
        let mut votes: Vec<(String, usize)> = Vec::new();
        for worktree in crate::model::parse_worktree_list(&porcelain) {
            if worktree.branch.is_empty() || worktree.path.trim_end_matches('/') == ctx.repo {
                continue;
            }
            let Some(template) = generalize(&worktree.path, &worktree.branch) else {
                continue;
            };
            match votes.iter_mut().find(|(t, _)| *t == template) {
                Some(vote) => vote.1 += 1,
                None => votes.push((template, 1)),
            }
        }
        votes.sort_by(|a, b| b.1.cmp(&a.1));
        let (template, count) = votes.first()?;
        // A tie means the repo has no single layout; leave it to the default.
        if votes.get(1).map(|(_, c)| c >= count).unwrap_or(false) {
            return None;
        }
        let plural = if *count == 1 { "worktree" } else { "worktrees" };
        Some(Detected::new(
            self.id(),
            Scope::Observed,
            format!("matches {count} existing {plural}"),
            template,
        ))
    }
}

/// A gitignored worktree directory is the repo declaring where they belong,
/// even before the first one is created.
struct IgnoreHint;

/// Directory names that only ever mean "worktrees live here", most specific first.
const IGNORE_HINTS: &[&str] = &[
    ".worktrees",
    "worktrees",
    ".wt",
    ".trees",
    "trees",
    ".claude/worktrees",
];

impl Source for IgnoreHint {
    fn id(&self) -> &'static str {
        "gitignore"
    }

    fn worktree_path(&self, ctx: &RepoCtx) -> Option<Detected> {
        let repo = Path::new(&ctx.repo);
        let mut entries = Vec::new();
        for file in [repo.join(".gitignore"), repo.join(".git/info/exclude")] {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                let entry = line.trim_matches('/');
                entries.push((entry.to_string(), display(&file)));
            }
        }
        let (entry, detail) = IGNORE_HINTS
            .iter()
            .find_map(|hint| entries.iter().find(|(e, _)| e == hint))?;
        Some(Detected::new(
            self.id(),
            Scope::Hint,
            format!("{detail} ignores {entry}/"),
            format!("{{{{ repo_path }}}}/{entry}/{{{{ branch | sanitize }}}}"),
        ))
    }
}

// ---------------------------------------------------------------------------
// Template translation
// ---------------------------------------------------------------------------

/// Rewrite every `{{ … }}` placeholder through `map`. A placeholder `map`
/// rejects (or an unterminated one) aborts the whole translation, so a template
/// is either reproduced exactly or not adopted at all — a *nearly* right path is
/// worse than falling through to the next source.
fn rewrite(template: &str, map: &dyn Fn(&str, Option<&str>) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}")?;
        let inner = after[..end].trim();
        let (var, filter) = match inner.split_once('|') {
            Some((v, f)) => (v.trim(), Some(f.trim())),
            None => (inner, None),
        };
        out.push_str(&map(var, filter)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Some(out)
}

/// Emit one of this plugin's own placeholders, preserving a `sanitize` filter.
fn placeholder(name: &str, filter: Option<&str>) -> Option<String> {
    match filter {
        None => Some(format!("{{{{ {name} }}}}")),
        Some("sanitize") => Some(format!("{{{{ {name} | sanitize }}}}")),
        Some(_) => None,
    }
}

/// A value that is constant for this repo can be baked into the template.
fn literal(value: &str, filter: Option<&str>) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    match filter {
        None => Some(value.to_string()),
        Some("sanitize") => Some(util::sanitize(value)),
        Some(_) => None,
    }
}

/// Translate a Worktrunk template. Its `sanitize_db`, `codename` and `hash_port`
/// filters have no equivalent here, so templates using them are skipped.
fn worktrunk_template(template: &str, ctx: &RepoCtx) -> Option<String> {
    rewrite(template, &|var, filter| match var {
        "repo_path" => placeholder("repo_path", filter),
        "repo" => placeholder("repo_name", filter),
        "branch" => placeholder("branch", filter),
        "owner" => literal(&ctx.owner, filter),
        _ => None,
    })
}

/// Translate a gwq Go-template naming scheme.
fn gwq_template(template: &str, ctx: &RepoCtx) -> Option<String> {
    rewrite(template, &|var, filter| {
        if filter.is_some() {
            return None;
        }
        match var.trim_start_matches('.') {
            "Host" => literal(&ctx.host, None),
            "Owner" => literal(&ctx.owner, None),
            "Repository" => placeholder("repo_name", None),
            // gwq sanitizes branch names for the filesystem the same way.
            "Branch" => placeholder("branch", Some("sanitize")),
            _ => None,
        }
    })
}

/// Turn one existing worktree into the template that would have produced it.
fn generalize(path: &str, branch: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    // A nested layout keeps the branch's slashes as directories.
    if branch.contains('/') {
        if let Some(prefix) = path.strip_suffix(&format!("/{branch}")) {
            return Some(format!("{prefix}/{{{{ branch }}}}"));
        }
    }
    let (dir, leaf) = path.rsplit_once('/')?;
    let short = branch.rsplit('/').next().unwrap_or(branch);
    let variants = [
        (branch.to_string(), "{{ branch }}"),
        (util::sanitize(branch), "{{ branch | sanitize }}"),
        (short.to_string(), "{{ branch_short }}"),
        (util::sanitize(short), "{{ branch_short | sanitize }}"),
    ];
    for (variant, placeholder) in variants {
        if variant.is_empty() {
            continue;
        }
        if let Some(at) = leaf.rfind(&variant) {
            let head = &leaf[..at];
            let tail = &leaf[at + variant.len()..];
            return Some(format!("{dir}/{head}{placeholder}{tail}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
fn config_home() -> PathBuf {
    env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env_var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_tilde(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    match env_var("HOME") {
        Some(home) => format!("{home}{rest}"),
        None => path.to_string(),
    }
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

fn join(base: &str, tail: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        tail.trim_start_matches('/')
    )
}

/// Render a config path with `$HOME` abbreviated, for `detect` output.
fn display(path: &Path) -> String {
    let path = path.to_string_lossy().into_owned();
    match env_var("HOME") {
        Some(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path,
    }
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn str_at(value: &toml::Value, key: &str) -> Option<String> {
    Some(value.get(key)?.as_str()?.to_string())
}

fn git_config_local(repo: &str, key: &str) -> Option<String> {
    let value = crate::git::git_stdout(&["-C", repo, "config", "--local", "--get", key]);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn git_config(repo: &str, key: &str) -> Option<String> {
    let value = crate::git::git_stdout(&["-C", repo, "config", "--get", key]);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The `[projects."…"]` entry naming this repo, preferring the most specific key.
fn project_table<'a>(value: &'a toml::Value, ctx: &RepoCtx) -> Option<(String, &'a toml::Value)> {
    let projects = value.get("projects")?.as_table()?;
    projects
        .iter()
        .filter(|(key, _)| ctx.matches(key))
        .max_by_key(|(key, _)| key.len())
        .map(|(key, table)| (key.clone(), table))
}

/// Compare project keys ignoring case, a `.git` suffix and surrounding slashes.
fn normalize_key(key: &str) -> String {
    let key = key.trim().trim_matches('/');
    let key = key.strip_suffix(".git").unwrap_or(key);
    key.to_lowercase()
}

/// Split a remote URL into (host, owner, name), for scp-style and URL forms.
fn parse_remote(url: &str) -> (String, String, String) {
    let url = url.trim();
    if url.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let rest = match url.find("://") {
        Some(at) => &url[at + 3..],
        None => url,
    };
    let rest = match rest.rsplit_once('@') {
        Some((_, after)) => after,
        None => rest,
    };
    let (host, path) = match rest.find([':', '/']) {
        Some(at) => (rest[..at].to_string(), &rest[at + 1..]),
        None => return (String::new(), String::new(), String::new()),
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let name = segments.last().copied().unwrap_or_default().to_string();
    let owner = if segments.len() >= 2 {
        segments[segments.len() - 2].to_string()
    } else {
        String::new()
    };
    (host, owner, name)
}

/// Find the first string containing `{branch}` anywhere in a JSON document.
fn find_branch_pattern(value: &serde_json::Value, depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    match value {
        serde_json::Value::String(s) if s.contains("{branch}") => Some(s.clone()),
        serde_json::Value::Object(map) => {
            map.values().find_map(|v| find_branch_pattern(v, depth + 1))
        }
        serde_json::Value::Array(items) => {
            items.iter().find_map(|v| find_branch_pattern(v, depth + 1))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `herdr-worktrees detect` — show what every source thinks, and what wins.
pub fn run_cli(_args: &[String]) -> anyhow::Result<()> {
    let repo = crate::git::repo_root()?;
    let repo = repo.to_string_lossy().into_owned();
    let config = crate::config::Config::load()?;
    let ctx = RepoCtx::new(&repo);

    println!("repo    {}", ctx.repo);
    if !ctx.name.is_empty() {
        println!("remote  {}", ctx.keys.first().cloned().unwrap_or_default());
    }
    println!();
    println!("worktree-path");
    for (id, detected) in explain(&ctx) {
        match detected {
            Some(d) => println!("  ✓ {id:<10} {}\n      {}", d.detail, d.value),
            None => println!("  · {id:<10} no match"),
        }
    }
    println!();
    match &config.worktree_path {
        Some(value) => println!("  → {value}\n    (explicit config; detection unused)"),
        None if !config.auto_detect() => {
            println!(
                "  → {}\n    (auto-detect disabled)",
                config.worktree_path_template(&repo)
            )
        }
        None => match worktree_path(&ctx) {
            Some(d) => println!("  → {}\n    (detected from {})", d.value, d.source),
            None => println!(
                "  → {}\n    (built-in default)",
                config.worktree_path_template(&repo)
            ),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RepoCtx {
        RepoCtx {
            repo: "/home/dev/app".to_string(),
            repo_name: "app".to_string(),
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            name: "app".to_string(),
            keys: vec![
                "github.com/acme/app".to_string(),
                "acme/app".to_string(),
                "app".to_string(),
                "/home/dev/app".to_string(),
            ],
        }
    }

    #[test]
    fn parses_remote_urls() {
        for url in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app.git",
            "ssh://git@github.com/acme/app",
        ] {
            assert_eq!(
                parse_remote(url),
                (
                    "github.com".to_string(),
                    "acme".to_string(),
                    "app".to_string()
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn project_keys_match_loosely() {
        let ctx = ctx();
        assert!(ctx.matches("github.com/acme/app"));
        assert!(ctx.matches("ACME/app.git"));
        assert!(ctx.matches("/home/dev/app/"));
        assert!(!ctx.matches("acme/other"));
    }

    #[test]
    fn translates_worktrunk_templates() {
        let ctx = ctx();
        assert_eq!(
            worktrunk_template(
                "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}",
                &ctx
            )
            .as_deref(),
            Some("{{ repo_path }}/../{{ repo_name }}.{{ branch | sanitize }}")
        );
        assert_eq!(
            worktrunk_template("/wt/{{ owner }}/{{branch}}", &ctx).as_deref(),
            Some("/wt/acme/{{ branch }}")
        );
    }

    #[test]
    fn skips_templates_it_cannot_reproduce() {
        let ctx = ctx();
        // Filters and variables with no equivalent here must not be guessed at.
        assert_eq!(
            worktrunk_template("/wt/{{ branch | codename(2) }}", &ctx),
            None
        );
        assert_eq!(worktrunk_template("/wt/{{ hash_port }}", &ctx), None);
        assert_eq!(worktrunk_template("/wt/{{ branch", &ctx), None);
    }

    #[test]
    fn translates_gwq_templates() {
        assert_eq!(
            gwq_template(GWQ_DEFAULT_TEMPLATE, &ctx()).as_deref(),
            Some("github.com/acme/{{ repo_name }}/{{ branch | sanitize }}")
        );
    }

    #[test]
    fn generalizes_existing_worktrees() {
        assert_eq!(
            generalize("/home/dev/app/.worktrees/kees-fix", "kees/fix").as_deref(),
            Some("/home/dev/app/.worktrees/{{ branch | sanitize }}")
        );
        assert_eq!(
            generalize("/home/dev/app.fix", "fix").as_deref(),
            Some("/home/dev/app.{{ branch }}")
        );
        assert_eq!(
            generalize("/wt/feature/login", "feature/login").as_deref(),
            Some("/wt/{{ branch }}")
        );
        // A path that says nothing about the branch is not a pattern.
        assert_eq!(generalize("/scratch/tmp", "fix"), None);
    }

    #[test]
    fn generalizes_prefixed_branches_by_short_name() {
        assert_eq!(
            generalize("/home/dev/app/.worktrees/fix", "kees/fix").as_deref(),
            Some("/home/dev/app/.worktrees/{{ branch_short }}")
        );
    }
}
