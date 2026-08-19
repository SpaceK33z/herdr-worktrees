//! The worktree switch/create popup (`prefix+w`).

use crate::background;
use crate::config::{apply_branch_prefix, branch_short_name, Config};
use crate::git;
use crate::herdr;
use crate::model::{self, Engine};
use crate::pr;
use crate::remove;
use crate::render;
use crate::setup;
use crate::tty;
use crate::util;
use anyhow::{bail, Context as _, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
                    if !matches!(
                        entry_route(entry_kind),
                        EntryRoute::Create | EntryRoute::Section | EntryRoute::Unknown
                    ) {
                        open_pr_in_browser(branch, entry_kind, &repo);
                    }
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
                    route @ (EntryRoute::Create | EntryRoute::LocalBranch) => {
                        let Some((name, exact_branch)) =
                            selected_creation_target(route, &fzf_out.query, &sel_branch)
                        else {
                            return Ok(());
                        };
                        let base = base_override.as_deref().unwrap_or(&engine.base);
                        create_worktree(name, base, &config, &repo, dry_run, exact_branch);
                        return Ok(());
                    }
                    EntryRoute::RemoteBranch => {
                        checkout_remote_worktree(&sel_branch, &config, &repo, dry_run);
                        return Ok(());
                    }
                    EntryRoute::PullRequest => {
                        let Some(number) = parse_pr_query(&fzf_out.query) else {
                            return Ok(());
                        };
                        checkout_pr_worktree(number, &config, &repo, dry_run);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryRoute {
    Create,
    PullRequest,
    Worktree,
    LocalBranch,
    RemoteBranch,
    Section,
    Unknown,
}

fn entry_route(kind: &str) -> EntryRoute {
    match kind {
        "create" => EntryRoute::Create,
        "pr" => EntryRoute::PullRequest,
        "worktree" => EntryRoute::Worktree,
        "branch" => EntryRoute::LocalBranch,
        "remote" => EntryRoute::RemoteBranch,
        "section" => EntryRoute::Section,
        _ => EntryRoute::Unknown,
    }
}

/// Parse a typed pull-request reference: `123`, `#123`, or `pr:123`.
fn parse_pr_query(query: &str) -> Option<u32> {
    let trimmed = query.trim();
    let digits = trimmed
        .strip_prefix("pr:")
        .or_else(|| trimmed.strip_prefix("PR:"))
        .or_else(|| trimmed.strip_prefix('#'))
        .unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn selected_creation_target<'a>(
    route: EntryRoute,
    query: &'a str,
    selected_branch: &'a str,
) -> Option<(&'a str, bool)> {
    match route {
        EntryRoute::Create if !query.is_empty() => Some((query, false)),
        EntryRoute::LocalBranch => Some((selected_branch, true)),
        _ => None,
    }
}

const MAIN_FZF_SEARCH_ARGS: &[&str] = &["--disabled", "--no-tac"];
const INTERNAL_FZF_FILTER_ARGS: &[&str] =
    &["--no-extended", "--ansi", "--delimiter=\t", "--with-nth=6"];
const PICKER_FOOTER: &str = "\x1b[2menter\x1b[0m switch/create · \x1b[2mctrl-n\x1b[0m new · \x1b[2malt-enter\x1b[0m base… · \x1b[2mctrl-p\x1b[0m open PR · \x1b[2mctrl-d\x1b[0m delete · \x1b[2mctrl-r\x1b[0m refresh · \x1b[2mesc\x1b[0m close";

fn picker_footer(github_prs: bool, refreshed_at: Option<u128>) -> String {
    if !github_prs {
        return PICKER_FOOTER.to_string();
    }
    let refreshed = refreshed_at
        .map(|timestamp| render::relative_age((timestamp / 1000) as i64))
        .map(|age| {
            if age == "now" {
                age
            } else {
                format!("{age} ago")
            }
        })
        .unwrap_or_else(|| "not refreshed".to_string());
    format!("{PICKER_FOOTER} · \x1b[2mGitHub: {refreshed}\x1b[0m")
}

fn current_picker_footer(repo: &str, github_prs: bool) -> String {
    let refreshed_at = github_prs
        .then(|| pr::last_refresh_ms(&model::state_dir(), repo))
        .flatten();
    picker_footer(github_prs, refreshed_at)
}

pub fn run_footer(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("usage: picker-footer");
    }
    let repo = git::repo_root()?.to_string_lossy().into_owned();
    let config = Config::load()?;
    let github_prs = config.github_prs() && git::has_github_remote(&repo);
    print!("{}", current_picker_footer(&repo, github_prs));
    Ok(())
}

fn run_fzf(engine: &Engine, cur_path: &str) -> Result<Option<FzfOut>> {
    let list = model::render_fzf_lines(engine, false);
    let header = render::render_picker_header_with_options(engine.show_worktree_name);
    let footer = current_picker_footer(&engine.repo_path, engine.github_prs);

    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr-worktrees".to_string());
    let cache = PickerCache::create(&list)?;
    let escaped_exe = util::shell_escape(&exe);
    let escaped_cache = util::shell_escape(&cache.path().to_string_lossy());
    // fzf shell-quotes placeholder values. Keep {q} unquoted so the exact query
    // reaches each helper as one argv value, including an empty query.
    let cached_cmd = format!("{escaped_exe} picker-cache-list {escaped_cache} {{q}}");
    let refresh_cmd = format!("{escaped_exe} picker-cache-refresh {escaped_cache} {{q}}");
    let footer_cmd = format!("{escaped_exe} picker-footer");
    let bind = build_fzf_bind(&list, cur_path, &cached_cmd, &refresh_cmd, &footer_cmd);

    let mut child = std::process::Command::new("fzf")
        .args(MAIN_FZF_SEARCH_ARGS)
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
            &footer,
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

fn build_fzf_bind(
    list: &str,
    cur_path: &str,
    cached_cmd: &str,
    refresh_cmd: &str,
    footer_cmd: &str,
) -> String {
    // Draw the cheap local snapshot first, then atomically replace it with the
    // status/PR-enriched list. Git/model work only runs for load and ctrl-r.
    let load_action = match model::fzf_line_index(list, cur_path) {
        Some(idx) if !cur_path.is_empty() => {
            format!(
                "load:pos({idx})+unbind(load)+reload-sync({refresh_cmd})+transform-footer({footer_cmd})"
            )
        }
        _ => format!("load:unbind(load)+reload-sync({refresh_cmd})+transform-footer({footer_cmd})"),
    };
    // The main picker is search-disabled, so reload order is display order.
    // `first` only resets selection after the synchronous reload; it cannot
    // reorder the fuzzy-ranked real rows or their appended create row.
    format!(
        "{load_action},change:reload-sync({cached_cmd})+first,ctrl-r:reload-sync({refresh_cmd})+first+transform-footer({footer_cmd})"
    )
}

struct PickerCache {
    path: PathBuf,
}

impl PickerCache {
    fn create(contents: &str) -> Result<Self> {
        let path = unique_cache_path(std::env::temp_dir(), "cache")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating picker cache {}", path.display()))?;
        if let Err(error) = file.write_all(contents.as_bytes()) {
            let _ = std::fs::remove_file(&path);
            return Err(error).context("writing picker cache");
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PickerCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn unique_cache_path(directory: PathBuf, suffix: &str) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..100u8 {
        let path = directory.join(format!(
            "herdr-worktrees-picker-{}-{timestamp}-{attempt}.{suffix}",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!("could not allocate a unique picker cache path")
}

fn query_is_row_safe(query: &str) -> bool {
    !query.is_empty() && !query.chars().any(char::is_control)
}

fn render_query_aware_list(
    cached: &str,
    query: &str,
    colors: &crate::theme::ThemeColors,
    pr_checkout: bool,
) -> Result<String> {
    if !query_is_row_safe(query) {
        return Ok(cached.to_string());
    }

    let mut child = std::process::Command::new("fzf")
        .args(INTERNAL_FZF_FILTER_ARGS)
        .args(["--filter", query])
        // The interactive picker intentionally honors user defaults. This
        // internal ranker must not: its output is the picker's row protocol.
        .env_remove("FZF_DEFAULT_OPTS")
        .env_remove("FZF_DEFAULT_OPTS_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning fzf cache filter")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(cached.as_bytes())
            .context("writing fzf cache filter input")?;
    }
    let filtered = child
        .wait_with_output()
        .context("waiting for fzf cache filter")?;
    // fzf uses status 1 for a successful filter with no matches.
    if !filtered.status.success() && filtered.status.code() != Some(1) {
        bail!(
            "fzf cache filter failed: {}",
            String::from_utf8_lossy(&filtered.stderr).trim()
        );
    }

    let output =
        String::from_utf8(filtered.stdout).context("fzf cache filter output was not UTF-8")?;
    let restored = restore_ranked_rows(cached, &output)?;
    let mut out = restored;
    if pr_checkout {
        if let Some(number) = parse_pr_query(query) {
            out = append_pr_row(out, number, colors);
        }
    }
    Ok(append_create_row(out, query, colors))
}

fn six_field_row_key(row: &str) -> Option<&str> {
    if row.bytes().filter(|byte| *byte == b'\t').count() != 5 {
        return None;
    }
    row.rsplit_once('\t').map(|(key, _display)| key)
}

fn strip_terminal_sequences(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\x1b' {
            plain.push(bytes[index]);
            index += 1;
            continue;
        }

        index += 1;
        match bytes.get(index) {
            Some(b'[') => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\x07' {
                        index += 1;
                        break;
                    }
                    if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            Some(_) => index += 1,
            None => {}
        }
    }
    String::from_utf8(plain).expect("removing terminal escapes preserves UTF-8")
}

fn restore_ranked_rows(cached: &str, filtered: &str) -> Result<String> {
    let mut originals =
        std::collections::HashMap::<&str, std::collections::VecDeque<(&str, String)>>::new();
    for (index, row) in cached.lines().enumerate() {
        let key = six_field_row_key(row)
            .with_context(|| format!("cached picker row {} does not have six fields", index + 1))?;
        originals
            .entry(key)
            .or_default()
            .push_back((row, strip_terminal_sequences(row)));
    }

    let mut restored = String::with_capacity(filtered.len());
    for (index, row) in filtered.lines().enumerate() {
        let key = six_field_row_key(row).with_context(|| {
            format!("filtered picker row {} does not have six fields", index + 1)
        })?;
        let candidates = originals.get_mut(key).with_context(|| {
            format!(
                "filtered picker row {} did not match a cached row",
                index + 1
            )
        })?;
        let position = candidates
            .iter()
            .position(|(_original, plain)| plain == row)
            .with_context(|| {
                format!(
                    "filtered picker row {} did not match cached display text",
                    index + 1
                )
            })?;
        let (original, _plain) = candidates
            .remove(position)
            .expect("matched cached picker row position exists");
        restored.push_str(original);
        restored.push('\n');
    }
    Ok(restored)
}

fn append_create_row(
    mut filtered: String,
    query: &str,
    colors: &crate::theme::ThemeColors,
) -> String {
    if !filtered.is_empty() && !filtered.ends_with('\n') {
        filtered.push('\n');
    }
    let action = colors.worktrees.paint_bold("＋ create worktree");
    let display = format!("{query}  {action}");
    filtered.push_str(query);
    filtered.push_str("\t\tcreate\t\t\t");
    filtered.push_str(&display);
    filtered.push('\n');
    filtered
}

/// Prepend the checkout-PR action row. It comes first so typing a bare number
/// and pressing Enter deterministically checks out the PR rather than landing
/// on a fuzzy match. Field 0 is the decimal number so `ctrl-p` opens the PR.
fn append_pr_row(list: String, number: u32, colors: &crate::theme::ThemeColors) -> String {
    let action = colors.worktrees.paint_bold("⇄ checkout pull request");
    let display = format!("#{number}  {action}");
    let mut row = String::new();
    row.push_str(&number.to_string());
    row.push_str("\t\tpr\t\t\t");
    row.push_str(&display);
    row.push('\n');
    row.push_str(&list);
    row
}

fn atomic_replace_cache(path: &Path, contents: &str) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = unique_cache_path(directory.to_path_buf(), "refresh")?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("creating picker refresh {}", temp_path.display()))?;
        file.write_all(contents.as_bytes())
            .context("writing picker refresh")?;
        drop(file);
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("replacing picker cache {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn cache_helper_args(args: &[String]) -> Result<(&Path, &str)> {
    let [cache, query] = args else {
        bail!("picker cache helper expects <cache-path> <query>");
    };
    Ok((Path::new(cache), query))
}

/// Cheap fzf change handler: read the last rendered snapshot, fuzzy-rank its
/// real rows, and append the query-specific create action. It runs no Git or
/// model computation.
pub fn run_cached_list(args: &[String]) -> Result<()> {
    let (cache_path, query) = cache_helper_args(args)?;
    let cached = std::fs::read_to_string(cache_path)
        .with_context(|| format!("reading picker cache {}", cache_path.display()))?;
    let config = Config::load()?;
    let colors = crate::theme::ThemeColors::load();
    print!(
        "{}",
        render_query_aware_list(&cached, query, &colors, config.pr_checkout())?
    );
    Ok(())
}

/// Expensive fzf load/ctrl-r handler: rebuild the model, atomically publish the
/// real candidate cache, then fuzzy-rank it and append the current create row.
pub fn run_cache_refresh(args: &[String]) -> Result<()> {
    let (cache_path, query) = cache_helper_args(args)?;
    let repo = git::repo_root()?.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    let engine = model::compute_all(&repo, &config, &state_dir, false);
    let colors = crate::theme::ThemeColors::load();
    let refreshed = model::render_fzf_lines(&engine, false);
    atomic_replace_cache(cache_path, &refreshed)?;
    print!(
        "{}",
        render_query_aware_list(&refreshed, query, &colors, config.pr_checkout())?
    );
    Ok(())
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

/// Check out a GitHub pull request by number into a worktree.
fn checkout_pr_worktree(number: u32, config: &Config, repo: &str, dry_run: bool) {
    let Some(target) = pr::resolve_pr(number, repo) else {
        tty::err(&format!(
            "could not resolve pull request #{number} (requires an authenticated gh CLI)"
        ));
        tty::wait_key();
        return;
    };
    let branch = target.head_ref.as_str();

    // Fast path: the PR's branch already has a worktree — just switch to it.
    if let Some(path) = find_worktree_path(repo, branch) {
        switch_worktree(&path, branch, config, repo, dry_run);
        return;
    }

    // Fork PRs contain untrusted code and run the setup script in that
    // checkout; confirm before fetching.
    if target.is_cross_repo && !confirm_fork_checkout(number, branch) {
        return;
    }

    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let short = branch_short_name(branch, &prefix);
    let base = if target.base_ref.is_empty() {
        git::resolve_base_ref(config, repo, &git::current_branch())
    } else {
        format!("origin/{}", target.base_ref)
    };
    let path = config.render_worktree_path(branch, &short, &base, repo, &user);

    if dry_run {
        println!("checkout pull request #{number} ({branch}) at {path}");
        return;
    }

    let ok = if target.is_cross_repo {
        checkout_fork_pr(&target, number, &path, branch, repo)
    } else {
        checkout_same_repo_pr(&path, branch, repo)
    };
    if !ok {
        return;
    }

    open_and_setup_worktree(&path, branch, &base, repo, config);
}

fn confirm_fork_checkout(number: u32, branch: &str) -> bool {
    let prompt = format!(
        "checkout #{number} from a fork ({branch})?\n\n\
         this fetches code from a fork and then runs the setup script in that\n\
         checkout — only continue for PRs you trust.\n\n\
         press enter to confirm · esc to cancel"
    );
    tty::confirm(&prompt, false)
}

/// Same-repository PR: reuse an existing local branch, or fetch
/// `origin/<branch>` and create a tracking worktree from it.
fn checkout_same_repo_pr(path: &str, branch: &str, repo: &str) -> bool {
    if git::ref_exists(repo, &format!("refs/heads/{branch}")) {
        return git::git_inherit(&["-C", repo, "worktree", "add", path, branch]);
    }

    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    if !git::git_inherit(&["-C", repo, "fetch", "origin", &refspec]) {
        tty::err(&format!("could not fetch '{branch}' from origin"));
        tty::wait_key();
        return false;
    }
    git::git_inherit(&[
        "-C",
        repo,
        "worktree",
        "add",
        "-b",
        branch,
        "--track",
        path,
        &format!("origin/{branch}"),
    ])
}

/// Fork PR: fetch `refs/pull/N/head` into a temporary ref, then create a local
/// branch from it. Reuse an existing local branch only when it already points
/// at the PR head; never overwrite a divergent local branch.
fn checkout_fork_pr(
    target: &pr::PullRequestTarget,
    number: u32,
    path: &str,
    branch: &str,
    repo: &str,
) -> bool {
    let temp_ref = format!("refs/herdr-worktrees/pr-{number}");
    let refspec = format!("refs/pull/{number}/head:{temp_ref}");
    if !git::git_inherit(&["-C", repo, "fetch", "origin", &refspec]) {
        tty::err(&format!("could not fetch pull request #{number} from origin"));
        tty::wait_key();
        return false;
    }

    let local_ref = format!("refs/heads/{branch}");
    let result = if git::ref_exists(repo, &local_ref) {
        if git::ref_oid(repo, &local_ref).as_deref() == Some(target.head_oid.as_str()) {
            git::git_inherit(&["-C", repo, "worktree", "add", path, branch])
        } else {
            tty::err(&format!(
                "branch '{branch}' already exists locally at a different commit; not overwriting"
            ));
            tty::wait_key();
            false
        }
    } else {
        git::git_inherit(&["-C", repo, "worktree", "add", "-b", branch, path, &temp_ref])
    };

    let _ = git::delete_ref(repo, &temp_ref);
    result
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
        add_remote_tracking_worktree, append_create_row, append_pr_row, atomic_replace_cache,
        build_fzf_bind, entry_route, parse_pr_query, picker_footer, pr_head_name,
        render_query_aware_list, restore_ranked_rows, selected_creation_target, EntryRoute,
        PickerCache, INTERNAL_FZF_FILTER_ARGS, MAIN_FZF_SEARCH_ARGS, PICKER_FOOTER,
    };
    use std::io::Write as _;
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

    fn fzf_available() -> bool {
        Command::new("fzf")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn main_picker_disables_search_and_overrides_default_tac() {
        assert_eq!(MAIN_FZF_SEARCH_ARGS, ["--disabled", "--no-tac"]);
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }

        let mut child = Command::new("fzf")
            .args(MAIN_FZF_SEARCH_ARGS)
            .args(["--filter", "row"])
            .env("FZF_DEFAULT_OPTS", "--tac")
            .env_remove("FZF_DEFAULT_OPTS_FILE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"row-first\nrow-second\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "row-first\nrow-second\n"
        );
    }

    #[test]
    fn internal_filter_is_literal_and_uses_visible_protocol_field() {
        assert_eq!(
            INTERNAL_FZF_FILTER_ARGS,
            ["--no-extended", "--ansi", "--delimiter=\t", "--with-nth=6"]
        );
    }

    #[test]
    fn enrichment_binding_keeps_initial_position_and_uses_query_aware_commands() {
        let list = "main\t/repo\tworktree\nother\t/wt\tworktree\n";
        let bind = build_fzf_bind(
            list,
            "/wt",
            "picker-cache-list cache {q}",
            "picker-cache-refresh cache {q}",
            "picker-footer",
        );
        assert!(bind.starts_with(
            "load:pos(2)+unbind(load)+reload-sync(picker-cache-refresh cache {q})+transform-footer(picker-footer)"
        ));
        assert!(bind.contains("change:reload-sync(picker-cache-list cache {q})+first"));
        assert!(bind.contains(
            "ctrl-r:reload-sync(picker-cache-refresh cache {q})+first+transform-footer(picker-footer)"
        ));
        assert_eq!(bind.matches("picker-cache-list").count(), 1);
        assert_eq!(bind.matches("picker-cache-refresh").count(), 2);
        assert_eq!(bind.matches("transform-footer").count(), 2);
    }

    #[test]
    fn github_refresh_status_is_subtle_and_optional() {
        assert_eq!(picker_footer(false, None), PICKER_FOOTER);
        let pending = picker_footer(true, None);
        assert!(pending.contains("\x1b[2mGitHub: not refreshed\x1b[0m"));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let fresh = picker_footer(true, Some(now));
        assert!(fresh.contains("\x1b[2mGitHub: now\x1b[0m"));
    }

    #[test]
    fn query_aware_list_ranks_real_rows_then_appends_styled_create_row() {
        let colors = crate::theme::ThemeColors::default();
        let cached = concat!(
            "weak\t/weak\tbranch\tclean\tclean\tproject foo suffix\n",
            "strong\t/strong\tbranch\tclean\tclean\tfoo\n",
            "miss\t/miss\tbranch\tclean\tclean\tbar\n",
        );
        assert_eq!(
            render_query_aware_list(cached, "", &colors, false).unwrap(),
            cached
        );

        let filtered = concat!(
            "strong\t/strong\tbranch\tclean\tclean\tfoo\n",
            "weak\t/weak\tbranch\tclean\tclean\tproject foo suffix\n",
        );
        let rendered = append_create_row(filtered.to_string(), "foo", &colors);
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].split('\t').next(), Some("strong"));
        assert_eq!(lines[1].split('\t').next(), Some("weak"));

        let fields: Vec<_> = lines.last().unwrap().split('\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], "foo");
        assert_eq!(fields[1], "");
        assert_eq!(fields[2], "create");
        assert_eq!(fields[3], "");
        assert_eq!(fields[4], "");
        assert!(fields[5].contains("＋ create worktree"));
        assert!(fields[5].contains("\x1b[1;"));
    }

    #[test]
    fn real_filter_restores_ranked_ansi_and_osc_rows() {
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }

        let colors = crate::theme::ThemeColors::default();
        let weak = "weak\t/weak\tbranch\tclean\tclean\t\x1b]8;;https://example.test/weak\x1b\\\x1b[31mproject foo suffix\x1b[0m\x1b]8;;\x1b\\";
        let strong = "strong\t/strong\tbranch\tclean\tclean\t\x1b]8;;https://example.test/strong\x1b\\\x1b[32mfoo\x1b[0m\x1b]8;;\x1b\\";
        let cached = format!("{weak}\n{strong}\n");

        let rendered = render_query_aware_list(&cached, "foo", &colors, false).unwrap();
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines[0], strong);
        assert_eq!(lines[1], weak);
        assert_eq!(lines[2].split('\t').nth(2), Some("create"));
        assert!(lines[0].contains("\x1b[32m"));
        assert!(lines[0].contains("\x1b]8;;https://example.test/strong\x1b\\"));
        assert!(lines[1].contains("\x1b[31m"));
        assert!(lines[1].contains("\x1b]8;;https://example.test/weak\x1b\\"));
    }

    #[test]
    fn ranked_row_restoration_handles_duplicate_keys_deterministically() {
        let cached = concat!(
            "same\t/path\tbranch\tclean\tclean\t\x1b[31mfirst\x1b[0m\n",
            "same\t/path\tbranch\tclean\tclean\t\x1b[32msecond\x1b[0m\n",
        );
        let filtered = concat!(
            "same\t/path\tbranch\tclean\tclean\tsecond\n",
            "same\t/path\tbranch\tclean\tclean\tfirst\n",
        );
        let restored = concat!(
            "same\t/path\tbranch\tclean\tclean\t\x1b[32msecond\x1b[0m\n",
            "same\t/path\tbranch\tclean\tclean\t\x1b[31mfirst\x1b[0m\n",
        );
        assert_eq!(restore_ranked_rows(cached, filtered).unwrap(), restored);
        assert!(restore_ranked_rows(cached, "missing\t/key\tbranch\t\t\trow\n").is_err());
    }

    #[test]
    fn query_aware_list_returns_only_create_for_no_match() {
        let colors = crate::theme::ThemeColors::default();
        let rendered = append_create_row(String::new(), "no-such-branch", &colors);
        assert_eq!(rendered.lines().count(), 1);
        assert_eq!(rendered.split('\t').nth(2), Some("create"));
    }

    #[test]
    fn query_aware_list_treats_operator_like_queries_literally_and_appends_create() {
        let colors = crate::theme::ThemeColors::default();
        for query in ["foo$", "!foo", "^foo"] {
            let filtered = format!("literal\t/literal\tbranch\tclean\tclean\t{query}\n");
            let rendered = append_create_row(filtered, query, &colors);
            let lines: Vec<_> = rendered.lines().collect();
            assert_eq!(lines.len(), 2, "query {query:?}: {rendered:?}");
            assert_eq!(lines[0].split('\t').next(), Some("literal"));
            let create: Vec<_> = lines[1].split('\t').collect();
            assert_eq!(create.len(), 6);
            assert_eq!(create[0], query);
            assert_eq!(create[2], "create");
        }
    }

    #[test]
    fn query_aware_list_omits_create_for_unsafe_queries() {
        let colors = crate::theme::ThemeColors::default();
        let cached = "main\t/repo\tworktree\tclean\tclean\tmain\n";
        for unsafe_query in ["tab\there", "line\nfeed", "carriage\rreturn", "escape\x1b"] {
            let rendered = render_query_aware_list(cached, unsafe_query, &colors, false).unwrap();
            assert_eq!(rendered, cached);
            assert!(rendered.lines().all(|line| line.split('\t').count() == 6));
        }
    }

    #[test]
    fn pr_query_parses_numbers_hash_and_prefix() {
        assert_eq!(parse_pr_query("123"), Some(123));
        assert_eq!(parse_pr_query("#123"), Some(123));
        assert_eq!(parse_pr_query("pr:123"), Some(123));
        assert_eq!(parse_pr_query("PR:123"), Some(123));
        assert_eq!(parse_pr_query(" 42 "), Some(42));
        assert_eq!(parse_pr_query(""), None);
        assert_eq!(parse_pr_query("123abc"), None);
        assert_eq!(parse_pr_query("feature/123"), None);
        assert_eq!(parse_pr_query("4294967296"), None); // exceeds u32
    }

    #[test]
    fn append_pr_row_prepends_before_existing_rows() {
        let colors = crate::theme::ThemeColors::default();
        let list = append_pr_row(
            "branch\t/p\tbranch\tclean\tclean\tbranch\n".to_string(),
            7,
            &colors,
        );
        let lines: Vec<_> = list.lines().collect();
        assert_eq!(lines.len(), 2);
        let fields: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], "7");
        assert_eq!(fields[1], "");
        assert_eq!(fields[2], "pr");
        assert!(fields[5].contains("⇄ checkout pull request"));
        assert_eq!(lines[1].split('\t').next(), Some("branch"));
    }

    #[test]
    fn pr_row_is_prepended_when_enabled_and_omitted_when_disabled() {
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }
        let colors = crate::theme::ThemeColors::default();
        let cached = "main\t/repo\tworktree\tclean\tclean\tmain\n";

        let enabled = render_query_aware_list(cached, "123", &colors, true).unwrap();
        let lines: Vec<_> = enabled.lines().collect();
        assert_eq!(lines[0].split('\t').nth(2), Some("pr"));
        assert_eq!(lines[0].split('\t').next(), Some("123"));
        assert!(lines[0].contains("checkout pull request"));
        assert_eq!(lines[1].split('\t').nth(2), Some("create"));

        let disabled = render_query_aware_list(cached, "123", &colors, false).unwrap();
        let lines: Vec<_> = disabled.lines().collect();
        assert_eq!(lines.len(), 1); // create row only, no PR row
        assert!(lines.iter().all(|line| line.split('\t').nth(2) != Some("pr")));
    }

    #[test]
    fn create_entry_uses_exact_query_with_ctrl_n_creation_semantics() {
        assert_eq!(entry_route("create"), EntryRoute::Create);
        assert_eq!(
            selected_creation_target(EntryRoute::Create, "typed/exact", "stale-selection"),
            Some(("typed/exact", false))
        );
        assert_eq!(
            selected_creation_target(EntryRoute::LocalBranch, "typed/exact", "existing"),
            Some(("existing", true))
        );
    }

    #[test]
    fn picker_cache_replacement_is_atomic_and_drop_cleans_up() {
        let cache = PickerCache::create("old\n").unwrap();
        let path = cache.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        atomic_replace_cache(&path, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        drop(cache);
        assert!(!path.exists());
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
