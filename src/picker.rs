//! The worktree switch/create popup (`prefix+w`).

use crate::background;
use crate::config::{apply_branch_prefix, branch_short_name, Config};
use crate::create;
use crate::git;
use crate::herdr;
use crate::model::{self, Engine};
use crate::pr;
use crate::remove;
use crate::render;
use crate::row::{self, PickerRow};
use crate::setup;
use crate::tty;
use crate::update;
use crate::util;
use anyhow::{anyhow, bail, Context as _, Result};
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

    if let Some(name) = dry_name.as_deref().filter(|_| dry_run) {
        return dry_run_name(&config, &repo, name);
    }

    let mut base_override: Option<String> = None;
    if base_mode {
        let base = git::resolve_base_ref(&config, &repo, &git::current_branch());
        base_override = pick_base(&repo, &base);
        if base_override.is_none() {
            return Ok(());
        }
    }

    let context = PickerContext {
        config: &config,
        repo: &repo,
        dry_run,
    };

    loop {
        let engine = model::compute_picker_initial(&repo, &config, &state_dir);
        let Some(fzf_out) = run_fzf(&engine, &cur_path)? else {
            return Ok(());
        };

        // esc / no input at all -> close, layout untouched
        if fzf_out.query.is_empty() && fzf_out.selection.is_none() {
            return Ok(());
        }

        let selection = fzf_out.selection.as_deref().map(PickerRow::parse_selection);
        let query = fzf_out.query.as_str();
        let base = base_override.as_deref().unwrap_or(&engine.base);
        let outcome = match fzf_out.key.as_str() {
            "ctrl-p" => open_selected_pr(selection, &repo),
            "ctrl-d" => delete_selected(selection, &context),
            "ctrl-u" => update_selected(selection, &context, base, false),
            "alt-u" => update_selected(selection, &context, base, true),
            "ctrl-n" => create_from_query(&context, query, base),
            "alt-enter" => create_on_picked_base(
                &context,
                query,
                base_override.as_deref(),
                engine.base.as_str(),
            ),
            _ => route_selection(&context, selection, query, base),
        };
        match report(outcome) {
            AfterAction::Redraw => continue,
            AfterAction::Close => return Ok(()),
        }
    }
}

/// What every picker action needs to know: which repository it acts on, how new
/// worktrees are named and opened, and whether this run only says what it would
/// do.
struct PickerContext<'a> {
    config: &'a Config,
    repo: &'a str,
    dry_run: bool,
}

/// Whether the popup closes after an action, or redraws and stays open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterAction {
    Close,
    Redraw,
}

/// The one place that decides how a failed action looks: the actions themselves
/// only report git's own output on the way past. A message the user cannot read
/// is no message, so a failure holds the popup open until a key is pressed.
fn report(outcome: Result<AfterAction>) -> AfterAction {
    match outcome {
        Ok(after) => after,
        Err(error) => {
            tty::err(&format!("{error:#}"));
            tty::wait_key();
            AfterAction::Close
        }
    }
}

/// ctrl-p: open the selected row's pull request in the browser.
fn open_selected_pr(selection: Option<PickerRow>, repo: &str) -> Result<AfterAction> {
    let Some(row) = selection.filter(|row| !row.branch.is_empty()) else {
        return Ok(AfterAction::Close);
    };
    if !matches!(
        entry_route(row.entry_kind),
        EntryRoute::Create | EntryRoute::Section | EntryRoute::Unknown
    ) {
        open_pr_in_browser(row.branch, row.entry_kind, repo)?;
    }
    Ok(AfterAction::Close)
}

/// ctrl-d: delete the selected worktree, then redraw the picker in place.
fn delete_selected(selection: Option<PickerRow>, context: &PickerContext) -> Result<AfterAction> {
    // Guard the action boundary, before confirmation, logs, panes or worker launch.
    if context.dry_run {
        return Ok(AfterAction::Redraw);
    }
    if let Some(row) = selection.filter(|row| {
        entry_route(row.entry_kind) == EntryRoute::Worktree
            && !row.path.is_empty()
            && row.path != context.repo
    }) {
        remove::delete_worktree(
            row.branch,
            row.path,
            context.config,
            context.repo,
            remove::RemoveOptions::default(),
        )?;
    }
    Ok(AfterAction::Redraw)
}

/// ctrl-u (alt-u to pick the base first): bring the base branch into the
/// selected worktree. Only a real checkout has something to update, so every
/// other row is a no-op that leaves the popup as it was.
fn update_selected(
    selection: Option<PickerRow>,
    context: &PickerContext,
    base: &str,
    pick: bool,
) -> Result<AfterAction> {
    let Some(row) = selection
        .filter(|row| entry_route(row.entry_kind) == EntryRoute::Worktree && !row.path.is_empty())
    else {
        return Ok(AfterAction::Redraw);
    };
    let base = if pick {
        // Cancelling the base prompt cancels the update.
        match pick_base(context.repo, base) {
            Some(base) => base,
            None => return Ok(AfterAction::Redraw),
        }
    } else {
        base.to_string()
    };
    let outcome = update::update_worktree(
        row.path,
        row.branch,
        &base,
        context.config,
        context.repo,
        context.dry_run,
    )?;
    // A conflict hands the worktree to an agent and focuses it; staying open
    // over that would only cover it up.
    Ok(match outcome {
        update::Outcome::Conflicted { .. } => AfterAction::Close,
        _ => AfterAction::Redraw,
    })
}

/// ctrl-n: create the typed query even when fzf highlights a fuzzy match.
fn create_from_query(context: &PickerContext, query: &str, base: &str) -> Result<AfterAction> {
    if !query.is_empty() {
        create_worktree(context, query, base, false)?;
    }
    Ok(AfterAction::Close)
}

/// alt-enter: always create, off a base branch picked for this run.
fn create_on_picked_base(
    context: &PickerContext,
    query: &str,
    base_override: Option<&str>,
    default_base: &str,
) -> Result<AfterAction> {
    if query.is_empty() {
        return Ok(AfterAction::Close);
    }
    let base = match base_override {
        Some(base) => base.to_string(),
        // Cancelling the base prompt cancels the creation.
        None => match pick_base(context.repo, default_base) {
            Some(base) => base,
            None => return Ok(AfterAction::Close),
        },
    };
    create_worktree(context, query, &base, false)?;
    Ok(AfterAction::Close)
}

/// enter: act on the highlighted row — or create the query when nothing at all
/// matched it.
fn route_selection(
    context: &PickerContext,
    selection: Option<PickerRow>,
    query: &str,
    base: &str,
) -> Result<AfterAction> {
    let Some(row) = selection else {
        if !query.is_empty() {
            create_worktree(context, query, base, false)?;
        }
        return Ok(AfterAction::Close);
    };
    match entry_route(row.entry_kind) {
        EntryRoute::Worktree => switch_worktree(context, row.path, row.branch)?,
        route @ (EntryRoute::Create | EntryRoute::LocalBranch) => {
            if let Some((name, exact_branch)) = selected_creation_target(route, query, row.branch) {
                create_worktree(context, name, base, exact_branch)?;
            }
        }
        EntryRoute::RemoteBranch => checkout_remote_worktree(context, row.branch)?,
        EntryRoute::PullRequest => {
            if let Some(number) = parse_pr_query(query) {
                checkout_pr_worktree(context, number)?;
            }
        }
        EntryRoute::Section => return Ok(AfterAction::Redraw),
        EntryRoute::Unknown => {}
    }
    Ok(AfterAction::Close)
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
        row::KIND_CREATE => EntryRoute::Create,
        row::KIND_PR => EntryRoute::PullRequest,
        row::KIND_WORKTREE => EntryRoute::Worktree,
        row::KIND_BRANCH => EntryRoute::LocalBranch,
        row::KIND_REMOTE => EntryRoute::RemoteBranch,
        row::KIND_SECTION => EntryRoute::Section,
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
    &["--no-extended", "--ansi", row::DELIMITER, row::WITH_NTH];
const PICKER_FOOTER: &str = "\x1b[2menter\x1b[0m switch/create · \x1b[2mctrl-n\x1b[0m new · \x1b[2malt-enter\x1b[0m base… · \x1b[2mctrl-u\x1b[0m update · \x1b[2mctrl-p\x1b[0m open PR · \x1b[2mctrl-d\x1b[0m delete · \x1b[2mctrl-r\x1b[0m refresh · \x1b[2mctrl-f\x1b[0m fetch · \x1b[2mesc\x1b[0m close";

fn picker_footer(github_prs: bool, status: pr::RefreshStatus) -> String {
    if !github_prs {
        return PICKER_FOOTER.to_string();
    }
    if status.failed {
        // Empty PR columns after a failed fetch otherwise read as "no pull
        // requests"; this is the only place the user can learn otherwise.
        return format!("{PICKER_FOOTER} · \x1b[31mGitHub: failed — check gh auth\x1b[0m");
    }
    let refreshed = status
        .refreshed_at
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
    let status = if github_prs {
        pr::refresh_status(&model::state_dir(), repo)
    } else {
        pr::RefreshStatus::default()
    };
    picker_footer(github_prs, status)
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

/// The helper invocations fzf runs for us, already shell-escaped.
struct PickerCommands {
    /// Keystroke handler: rank the last published snapshot, no Git work.
    cached: String,
    /// First draw: rebuild the list, reusing fresh cached GitHub data.
    load: String,
    /// ctrl-r: rebuild the list, bypassing the GitHub cache.
    refresh: String,
    /// ctrl-f: `git fetch origin`, then a cache-bypassing rebuild.
    fetch: String,
    footer: String,
}

impl PickerCommands {
    fn new(exe: &str, cache: &Path) -> Self {
        let exe = util::shell_escape(exe);
        let cache = util::shell_escape(&cache.to_string_lossy());
        // fzf shell-quotes placeholder values. Keep {q} unquoted so the exact
        // query reaches each helper as one argv value, including an empty query.
        Self {
            cached: format!("{exe} picker-cache-list {cache} {{q}}"),
            load: format!("{exe} picker-cache-refresh --cached {cache} {{q}}"),
            refresh: format!("{exe} picker-cache-refresh {cache} {{q}}"),
            fetch: format!("{exe} picker-cache-fetch {cache} {{q}}"),
            footer: format!("{exe} picker-footer"),
        }
    }
}

fn run_fzf(engine: &Engine, cur_path: &str) -> Result<Option<FzfOut>> {
    let list = model::render_fzf_lines(engine, false);
    let header = render::render_header_with_options(engine.show_worktree_name);
    let footer = current_picker_footer(&engine.repo_path, engine.github_prs);

    let cache = PickerCache::create(&list)?;
    let commands = PickerCommands::new(&util::self_exe(), cache.path());
    let bind = build_fzf_bind(&list, cur_path, &commands);

    let mut child = std::process::Command::new("fzf")
        .args(MAIN_FZF_SEARCH_ARGS)
        .args([
            "--print-query",
            "--expect=ctrl-n,alt-enter,ctrl-p,ctrl-d,ctrl-u,alt-u",
            row::DELIMITER,
            row::WITH_NTH,
            row::ACCEPT_NTH,
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

fn build_fzf_bind(list: &str, cur_path: &str, commands: &PickerCommands) -> String {
    let PickerCommands {
        cached,
        load,
        refresh,
        fetch,
        footer,
    } = commands;
    // Draw the cheap local snapshot first, then atomically replace it with the
    // status/PR-enriched list. Git/model work only runs for load, ctrl-r and
    // ctrl-f.
    let load_action = match model::fzf_line_index(list, cur_path) {
        Some(idx) if !cur_path.is_empty() => {
            format!("load:pos({idx})+unbind(load)+reload-sync({load})+transform-footer({footer})")
        }
        _ => format!("load:unbind(load)+reload-sync({load})+transform-footer({footer})"),
    };
    // The main picker is search-disabled, so reload order is display order.
    // `first` only resets selection after the synchronous reload; it cannot
    // reorder the fuzzy-ranked real rows or their appended create row.
    format!(
        "{load_action},change:reload-sync({cached})+first,\
         ctrl-r:reload-sync({refresh})+first+transform-footer({footer}),\
         ctrl-f:reload-sync({fetch})+first+transform-footer({footer})"
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
            if !list_shows_pr(&out, number) {
                out = prepend_pr_row(out, number, colors);
            }
        }
    }
    Ok(append_create_row(out, query, colors))
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
    for (index, line) in cached.lines().enumerate() {
        let key = row::key_fields(line).with_context(|| {
            format!(
                "cached picker row {} does not have {} fields",
                index + 1,
                row::FIELD_COUNT
            )
        })?;
        originals
            .entry(key)
            .or_default()
            .push_back((line, strip_terminal_sequences(line)));
    }

    let mut restored = String::with_capacity(filtered.len());
    for (index, line) in filtered.lines().enumerate() {
        let key = row::key_fields(line).with_context(|| {
            format!(
                "filtered picker row {} does not have {} fields",
                index + 1,
                row::FIELD_COUNT
            )
        })?;
        let candidates = originals.get_mut(key).with_context(|| {
            format!(
                "filtered picker row {} did not match a cached row",
                index + 1
            )
        })?;
        let position = candidates
            .iter()
            .position(|(_original, plain)| plain == line)
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
    PickerRow::action(query, row::KIND_CREATE, &display).write_line(&mut filtered);
    filtered
}

/// True when some ranked row already displays this pull request (its `#N`
/// cell): the PR is checked out, fetched to origin, or both, so picking that
/// row does the job and the synthesized checkout action would be redundant.
fn list_shows_pr(list: &str, number: u32) -> bool {
    list.lines()
        .filter_map(PickerRow::parse)
        .any(|row| row.pr_number == Some(number))
}

/// Prepend the checkout-PR action row. It comes first so typing a bare number
/// and pressing Enter deterministically checks out the PR rather than landing
/// on a fuzzy match. The branch field is the decimal number so `ctrl-p` opens
/// the PR.
fn prepend_pr_row(list: String, number: u32, colors: &crate::theme::ThemeColors) -> String {
    let action = colors.worktrees.paint_bold("⇄ checkout pull request");
    let display = format!("#{number}  {action}");
    let mut out = String::with_capacity(list.len() + display.len() + row::FIELD_COUNT);
    PickerRow::action(&number.to_string(), row::KIND_PR, &display).write_line(&mut out);
    out.push_str(&list);
    out
}

fn atomic_replace_cache(path: &Path, contents: &str) -> Result<()> {
    util::write_atomic(path, contents.as_bytes())
        .with_context(|| format!("replacing picker cache {}", path.display()))
}

fn cache_helper_args(args: &[String]) -> Result<(&Path, &str)> {
    let [cache, query] = args else {
        bail!("picker cache helper expects <cache-path> <query>");
    };
    Ok((Path::new(cache), query))
}

/// The refresh helper takes an optional leading `--cached`: fzf's `load` event
/// redraws on every open and may reuse fresh GitHub data, while `ctrl-r` is a
/// deliberate request for a new fetch.
fn refresh_helper_args(args: &[String]) -> Result<(&Path, &str, bool)> {
    let (use_cache, rest) = match args.split_first() {
        Some((first, rest)) if first == "--cached" => (true, rest),
        _ => (false, args),
    };
    let (cache, query) = cache_helper_args(rest)?;
    Ok((cache, query, use_cache))
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
    let (cache_path, query, use_cache) = refresh_helper_args(args)?;
    republish_cache(cache_path, query, use_cache)
}

/// ctrl-f handler: update the remote-tracking refs the sync and pull counts are
/// computed from, then rebuild as ctrl-r does. The fetch is best effort — the
/// list is still redrawn from local state when the remote is slow or gone.
pub fn run_cache_fetch(args: &[String]) -> Result<()> {
    let (cache_path, query) = cache_helper_args(args)?;
    let repo = git::repo_root()?.to_string_lossy().into_owned();
    git::git_timeout(
        &["-C", &repo, "fetch", "--quiet", "origin"],
        git::FETCH_TIMEOUT,
    );
    republish_cache(cache_path, query, false)
}

fn republish_cache(cache_path: &Path, query: &str, use_cache: bool) -> Result<()> {
    let repo = git::repo_root()?.to_string_lossy().into_owned();
    let config = Config::load()?;
    let state_dir = model::state_dir();
    let engine = model::compute_all(&repo, &config, &state_dir, use_cache);
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

fn open_pr_in_browser(branch: &str, entry_kind: &str, repo: &str) -> Result<()> {
    let head = pr_head_name(branch, entry_kind);
    let opened = std::process::Command::new("gh")
        .args(["pr", "view", head, "--web"])
        .current_dir(repo)
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        bail!("could not open a pull request for '{head}'");
    }
    Ok(())
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
    tty::pick(
        &branches.join("\n"),
        "base branch",
        "pick a base branch · esc to cancel",
        &util::strip_remote(base),
    )
}

/// Open the checkout in Herdr and return its root pane id (so the setup pane
/// can be split next to it).
fn open_worktree(path: &str, branch: &str, config: &Config, repo: &str) -> Option<String> {
    herdr::open_checkout(config.open_mode(), repo, path, branch)
}

fn switch_worktree(context: &PickerContext, path: &str, branch: &str) -> Result<()> {
    if context.dry_run {
        println!("switch to {branch} ({path})");
        return Ok(());
    }
    match herdr::worktree_workspace_id(path, context.repo) {
        Some(ws) => herdr::run(&["workspace".into(), "focus".into(), ws]),
        None => {
            let _ = open_worktree(path, branch, context.config, context.repo);
        }
    }
    Ok(())
}

fn checkout_remote_worktree(context: &PickerContext, remote: &str) -> Result<()> {
    let repo = context.repo;
    let config = context.config;
    let local_branch = model::origin_local_branch(remote)
        .with_context(|| format!("invalid origin branch '{remote}'"))?;
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let short = branch_short_name(local_branch, &prefix);
    let path = config.render_worktree_path(local_branch, &short, remote, repo, &user);

    if context.dry_run {
        println!("checkout {remote} as {local_branch} at {path}");
        return Ok(());
    }

    // Re-check the local ref at selection time. If it appeared since the picker
    // was rendered, use it rather than trying to replace it.
    if git::ref_exists(repo, &format!("refs/heads/{local_branch}")) {
        return create_worktree(context, local_branch, remote, true);
    }
    if !git::ref_exists(repo, &format!("refs/remotes/{remote}")) {
        bail!("remote branch '{remote}' no longer exists");
    }
    add_remote_tracking_worktree(repo, &path, local_branch, remote)?;

    open_and_setup_worktree(&path, local_branch, remote, repo, config);
    Ok(())
}

/// Check out a GitHub pull request by number into a worktree.
fn checkout_pr_worktree(context: &PickerContext, number: u32) -> Result<()> {
    let repo = context.repo;
    let config = context.config;
    let target = pr::resolve_pr_detailed(number, repo)
        .map_err(|error| anyhow!("could not resolve pull request #{number}: {error}"))?;
    let branch = target.head_ref.as_str();

    // A same-named fork branch is not the same branch. Validate provenance
    // and the pinned head even on the existing-worktree fast path.
    if let Some(path) = find_worktree_path(repo, branch) {
        verify_pr_checkout_identity(&target, branch, repo)?;
        return switch_worktree(context, &path, branch);
    }

    // Fork PRs contain untrusted code and run the setup script in that
    // checkout; confirm before fetching.
    if target.is_cross_repo && !context.dry_run && !confirm_fork_checkout(number, branch) {
        return Ok(());
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

    if context.dry_run {
        println!("checkout pull request #{number} ({branch}) at {path}");
        return Ok(());
    }

    let checked_out = if target.is_cross_repo {
        checkout_fork_pr(&target, number, &path, branch, repo)?
    } else {
        checkout_same_repo_pr(&target, &path, branch, repo)?
    };
    if !checked_out {
        return Ok(());
    }

    open_and_setup_worktree(&path, branch, &base, repo, config);
    Ok(())
}

fn verify_pr_checkout_identity(
    target: &pr::PullRequestTarget,
    branch: &str,
    repo: &str,
) -> Result<()> {
    let provenance = pr::fork_repository(repo, branch);
    let identity_matches = if target.is_cross_repo {
        provenance
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(&target.head_repository))
    } else {
        provenance.is_none()
    };
    if !identity_matches
        || (target.is_cross_repo
            && git::ref_oid(repo, &format!("refs/heads/{branch}")).as_deref()
                != Some(&target.head_oid))
    {
        bail!("local branch '{branch}' does not identify pull request #{} from {}; rename the unrelated branch before checkout", target.number, target.head_repository);
    }
    Ok(())
}

fn confirm_fork_checkout(number: u32, branch: &str) -> bool {
    let prompt = format!(
        "checkout #{number} from a fork ({branch})?\n\n\
         this fetches code from a fork and then runs the setup script in that\n\
         checkout — only continue for PRs you trust.\n\n\
         press enter to confirm · esc to cancel"
    );
    tty::confirm(&prompt)
}

/// A step the user is asked to approve. `Ok(false)` means they declined, which
/// is a cancellation rather than a failure to report.
type Confirmed = Result<bool>;

/// Same-repository PR: reuse an existing local branch, or fetch
/// `origin/<branch>` and create a tracking worktree from it.
fn checkout_same_repo_pr(
    target: &pr::PullRequestTarget,
    path: &str,
    branch: &str,
    repo: &str,
) -> Confirmed {
    if git::ref_exists(repo, &format!("refs/heads/{branch}")) {
        verify_pr_checkout_identity(target, branch, repo)?;
        if !reconcile_local_pr_branch(target, branch, repo)? {
            return Ok(false);
        }
        worktree_add(&["-C", repo, "worktree", "add", path, branch])?;
        return Ok(true);
    }

    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    if !git::git_inherit(&["-C", repo, "fetch", "origin", &refspec]) {
        bail!("could not fetch '{branch}' from origin");
    }
    worktree_add(&[
        "-C",
        repo,
        "worktree",
        "add",
        "-b",
        branch,
        "--track",
        path,
        &format!("origin/{branch}"),
    ])?;
    Ok(true)
}

/// A local branch left over from an earlier checkout can sit behind the pull
/// request's current head — checking it out as-is would silently present stale
/// commits as the PR. Fast-forward it when the PR only moved ahead; refuse when
/// the two have diverged, as the fork path does.
fn reconcile_local_pr_branch(
    target: &pr::PullRequestTarget,
    branch: &str,
    repo: &str,
) -> Confirmed {
    let local_ref = format!("refs/heads/{branch}");
    let Some(local_oid) = git::ref_oid(repo, &local_ref) else {
        return Ok(true);
    };
    if local_oid == target.head_oid {
        return Ok(true);
    }

    // The PR head may not be in this clone yet; the fetch also refreshes the
    // remote-tracking ref the new worktree is compared against.
    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    if !git::git_inherit(&["-C", repo, "fetch", "origin", &refspec])
        || !git::ref_exists(repo, &format!("{}^{{commit}}", target.head_oid))
    {
        bail!(
            "could not fetch pull request #{} ({branch}) from origin",
            target.number
        );
    }

    if !is_ancestor(repo, &local_oid, &target.head_oid) {
        bail!(
            "local branch '{branch}' has diverged from pull request #{}; not overwriting",
            target.number
        );
    }

    let prompt = format!(
        "local branch '{branch}' is behind pull request #{}.\n\n\
         fast-forward it to the pull request's head before checking it out?\n\n\
         press enter to confirm · esc to keep the local commits",
        target.number
    );
    if !tty::confirm(&prompt) {
        return Ok(false);
    }
    // The old value makes this a compare-and-swap: a concurrent update aborts
    // the fast-forward rather than discarding it.
    if !git::git_inherit(&[
        "-C",
        repo,
        "update-ref",
        &local_ref,
        &target.head_oid,
        &local_oid,
    ]) {
        bail!(
            "could not fast-forward '{branch}' to pull request #{}",
            target.number
        );
    }
    Ok(true)
}

fn is_ancestor(repo: &str, ancestor: &str, descendant: &str) -> bool {
    git::git_success(&[
        "-C",
        repo,
        "merge-base",
        "--is-ancestor",
        ancestor,
        descendant,
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
) -> Confirmed {
    let local_ref = format!("refs/heads/{branch}");
    if git::ref_exists(repo, &local_ref) {
        verify_pr_checkout_identity(target, branch, repo)?;
    }
    let temp_ref = format!("refs/herdr-worktrees/pr-{number}-{}", std::process::id());
    let refspec = format!("refs/pull/{number}/head:{temp_ref}");
    if !git::git_inherit(&["-C", repo, "fetch", "origin", &refspec]) {
        bail!("could not fetch pull request #{number} from origin");
    }

    let fetched = git::ref_oid(repo, &temp_ref);
    if fetched.as_deref() != Some(&target.head_oid) {
        let _ = git::delete_ref(repo, &temp_ref);
        bail!("pull request #{number} changed during fetch; retry checkout");
    }
    let result = if git::ref_exists(repo, &local_ref) {
        if git::ref_oid(repo, &local_ref).as_deref() == Some(target.head_oid.as_str()) {
            worktree_add(&["-C", repo, "worktree", "add", path, branch])
        } else {
            Err(anyhow!(
                "branch '{branch}' already exists locally at a different commit; not overwriting"
            ))
        }
    } else {
        worktree_add(&["-C", repo, "worktree", "add", "-b", branch, path, &temp_ref])
    };

    // The temporary ref goes regardless: it exists only for the `worktree add`
    // above, so `?` must not skip past this.
    let _ = git::delete_ref(repo, &temp_ref);
    result?;
    if !git::git_success(&[
        "-C",
        repo,
        "config",
        &format!("branch.{branch}.herdr-pr-repository"),
        &target.head_repository,
    ]) {
        bail!("checkout created, but could not record fork identity; setup was not run");
    }
    Ok(true)
}

use create::worktree_add;

fn add_remote_tracking_worktree(repo: &str, path: &str, local: &str, remote: &str) -> Result<()> {
    worktree_add(&[
        "-C", repo, "worktree", "add", "-b", local, "--track", path, remote,
    ])
}

fn create_worktree(
    context: &PickerContext,
    name: &str,
    base: &str,
    exact_branch: bool,
) -> Result<()> {
    let repo = context.repo;
    let config = context.config;
    if !context.dry_run {
        let created = create::create(config, repo, name, Some(base), exact_branch)?;
        open_and_setup_worktree(&created.path, &created.branch, &created.base, repo, config);
        return Ok(());
    }
    let user = git::resolve_user(repo);
    let prefix = config.resolved_prefix(&user);
    let final_branch = if exact_branch {
        name.to_string()
    } else {
        apply_branch_prefix(name, &prefix)
    };
    let short = branch_short_name(&final_branch, &prefix);
    let path = config.render_worktree_path(&final_branch, &short, base, repo, &user);

    if git::ref_exists(repo, &format!("refs/heads/{final_branch}")) {
        println!("checkout {final_branch} at {path}");
    } else {
        println!("create {final_branch} from {base} at {path}");
    }
    Ok(())
}

/// Open the checkout right away, then run setup in the existing split-pane or
/// detached fallback flow.
fn open_and_setup_worktree(path: &str, branch: &str, base: &str, repo: &str, config: &Config) {
    let shell_pane = open_worktree(path, branch, config, repo);

    if !setup::has_work(repo, config) {
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
    let exe = util::self_exe();
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
    let exe = util::self_exe();
    let log = background::log_path("setup");
    let args = [
        "setup-bg".to_string(),
        path.to_string(),
        branch.to_string(),
        base.to_string(),
        repo.to_string(),
    ];
    if background::spawn_detached(&exe, &args, &log).is_err() {
        let prepared = setup::run_setup(path, branch, base, repo, config);
        if !prepared.ok() {
            tty::err(&format!(
                "{} — worktree left in place at {path}",
                prepared.summary()
            ));
        }
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
        add_remote_tracking_worktree, append_create_row, atomic_replace_cache, build_fzf_bind,
        entry_route, list_shows_pr, parse_pr_query, picker_footer, pr_head_name, prepend_pr_row,
        refresh_helper_args, render_query_aware_list, restore_ranked_rows,
        selected_creation_target, EntryRoute, PickerCache, PickerCommands,
        INTERNAL_FZF_FILTER_ARGS, MAIN_FZF_SEARCH_ARGS, PICKER_FOOTER,
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
            ["--no-extended", "--ansi", "--delimiter=\t", "--with-nth=7"]
        );
    }

    fn commands() -> PickerCommands {
        PickerCommands::new("exe", std::path::Path::new("/tmp/cache"))
    }

    #[test]
    fn enrichment_binding_keeps_initial_position_and_uses_query_aware_commands() {
        let list = "main\t/repo\tworktree\nother\t/wt\tworktree\n";
        let bind = build_fzf_bind(list, "/wt", &commands());
        assert!(bind.starts_with(
            "load:pos(2)+unbind(load)+reload-sync('exe' picker-cache-refresh --cached '/tmp/cache' {q})+transform-footer('exe' picker-footer)"
        ));
        assert!(bind.contains("change:reload-sync('exe' picker-cache-list '/tmp/cache' {q})+first"));
        assert!(bind.contains(
            "ctrl-r:reload-sync('exe' picker-cache-refresh '/tmp/cache' {q})+first+transform-footer('exe' picker-footer)"
        ));
        assert!(bind.contains(
            "ctrl-f:reload-sync('exe' picker-cache-fetch '/tmp/cache' {q})+first+transform-footer('exe' picker-footer)"
        ));
        assert_eq!(bind.matches("picker-cache-list").count(), 1);
        assert_eq!(bind.matches("picker-cache-fetch").count(), 1);
        assert_eq!(bind.matches("transform-footer").count(), 3);
    }

    /// The load event redraws on every open, so it may reuse fresh cached
    /// GitHub data; ctrl-r and ctrl-f are deliberate requests for a new fetch.
    #[test]
    fn only_the_load_event_reuses_the_github_cache() {
        let commands = commands();
        assert!(commands.load.contains(" picker-cache-refresh --cached "));
        assert!(!commands.refresh.contains("--cached"));
        assert!(!commands.fetch.contains("--cached"));

        let cached = ["--cached".to_string(), "cache".to_string(), "q".to_string()];
        assert!(refresh_helper_args(&cached).unwrap().2);
        let plain = ["cache".to_string(), "q".to_string()];
        let (path, query, use_cache) = refresh_helper_args(&plain).unwrap();
        assert_eq!(path, std::path::Path::new("cache"));
        assert_eq!(query, "q");
        assert!(!use_cache);
        assert!(refresh_helper_args(&["cache".to_string()]).is_err());
    }

    #[test]
    fn github_refresh_status_is_subtle_and_optional() {
        let none = crate::pr::RefreshStatus::default();
        assert_eq!(picker_footer(false, none), PICKER_FOOTER);
        assert!(picker_footer(true, none).contains("\x1b[2mGitHub: not refreshed\x1b[0m"));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let fresh = picker_footer(
            true,
            crate::pr::RefreshStatus {
                refreshed_at: Some(now),
                failed: false,
            },
        );
        assert!(fresh.contains("\x1b[2mGitHub: now\x1b[0m"));
    }

    /// A failed fetch leaves the PR columns empty, which otherwise reads as
    /// "this repository has no pull requests".
    #[test]
    fn a_failed_github_fetch_is_called_out_in_the_footer() {
        let failed = picker_footer(
            true,
            crate::pr::RefreshStatus {
                refreshed_at: Some(1),
                failed: true,
            },
        );
        assert!(
            failed.contains("GitHub: failed — check gh auth"),
            "{failed}"
        );
        assert!(!failed.contains("ago"), "{failed}");
    }

    #[test]
    fn footer_documents_the_fetch_binding() {
        assert!(PICKER_FOOTER.contains("ctrl-f\x1b[0m fetch"));
    }

    #[test]
    fn query_aware_list_ranks_real_rows_then_appends_styled_create_row() {
        let colors = crate::theme::ThemeColors::default();
        let cached = concat!(
            "weak\t/weak\tbranch\tclean\tclean\t\tproject foo suffix\n",
            "strong\t/strong\tbranch\tclean\tclean\t\tfoo\n",
            "miss\t/miss\tbranch\tclean\tclean\t\tbar\n",
        );
        assert_eq!(
            render_query_aware_list(cached, "", &colors, false).unwrap(),
            cached
        );

        let filtered = concat!(
            "strong\t/strong\tbranch\tclean\tclean\t\tfoo\n",
            "weak\t/weak\tbranch\tclean\tclean\t\tproject foo suffix\n",
        );
        let rendered = append_create_row(filtered.to_string(), "foo", &colors);
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].split('\t').next(), Some("strong"));
        assert_eq!(lines[1].split('\t').next(), Some("weak"));

        let fields: Vec<_> = lines.last().unwrap().split('\t').collect();
        assert_eq!(fields.len(), 7);
        assert_eq!(fields[0], "foo");
        assert_eq!(fields[1], "");
        assert_eq!(fields[2], "create");
        assert_eq!(fields[3], "");
        assert_eq!(fields[4], "");
        assert!(fields[6].contains("＋ create worktree"));
        assert!(fields[6].contains("\x1b[1;"));
    }

    #[test]
    fn real_filter_restores_ranked_ansi_and_osc_rows() {
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }

        let colors = crate::theme::ThemeColors::default();
        let weak = "weak\t/weak\tbranch\tclean\tclean\t\t\x1b]8;;https://example.test/weak\x1b\\\x1b[31mproject foo suffix\x1b[0m\x1b]8;;\x1b\\";
        let strong = "strong\t/strong\tbranch\tclean\tclean\t\t\x1b]8;;https://example.test/strong\x1b\\\x1b[32mfoo\x1b[0m\x1b]8;;\x1b\\";
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
            "same\t/path\tbranch\tclean\tclean\t\t\x1b[31mfirst\x1b[0m\n",
            "same\t/path\tbranch\tclean\tclean\t\t\x1b[32msecond\x1b[0m\n",
        );
        let filtered = concat!(
            "same\t/path\tbranch\tclean\tclean\t\tsecond\n",
            "same\t/path\tbranch\tclean\tclean\t\tfirst\n",
        );
        let restored = concat!(
            "same\t/path\tbranch\tclean\tclean\t\t\x1b[32msecond\x1b[0m\n",
            "same\t/path\tbranch\tclean\tclean\t\t\x1b[31mfirst\x1b[0m\n",
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
            let filtered = format!("literal\t/literal\tbranch\tclean\tclean\t\t{query}\n");
            let rendered = append_create_row(filtered, query, &colors);
            let lines: Vec<_> = rendered.lines().collect();
            assert_eq!(lines.len(), 2, "query {query:?}: {rendered:?}");
            assert_eq!(lines[0].split('\t').next(), Some("literal"));
            let create: Vec<_> = lines[1].split('\t').collect();
            assert_eq!(create.len(), 7);
            assert_eq!(create[0], query);
            assert_eq!(create[2], "create");
        }
    }

    #[test]
    fn query_aware_list_omits_create_for_unsafe_queries() {
        let colors = crate::theme::ThemeColors::default();
        let cached = "main\t/repo\tworktree\tclean\tclean\t\tmain\n";
        for unsafe_query in ["tab\there", "line\nfeed", "carriage\rreturn", "escape\x1b"] {
            let rendered = render_query_aware_list(cached, unsafe_query, &colors, false).unwrap();
            assert_eq!(rendered, cached);
            assert!(rendered.lines().all(|line| line.split('\t').count() == 7));
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
    fn prepend_pr_row_puts_the_row_before_existing_rows() {
        let colors = crate::theme::ThemeColors::default();
        let list = prepend_pr_row(
            "branch\t/p\tbranch\tclean\tclean\t\tbranch\n".to_string(),
            7,
            &colors,
        );
        let lines: Vec<_> = list.lines().collect();
        assert_eq!(lines.len(), 2);
        let fields: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 7);
        assert_eq!(fields[0], "7");
        assert_eq!(fields[1], "");
        assert_eq!(fields[2], "pr");
        assert!(fields[6].contains("⇄ checkout pull request"));
        assert_eq!(lines[1].split('\t').next(), Some("branch"));
    }

    #[test]
    fn pr_action_row_is_skipped_when_a_row_already_shows_the_pr() {
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }
        let colors = crate::theme::ThemeColors::default();
        let cached = concat!(
            "main\t/repo\tworktree\tclean\tclean\t\tmain\n",
            "fix\t/fix\tremote\tclean\tclean\t123\torigin/fix  #123\n",
        );

        let rendered = render_query_aware_list(cached, "#123", &colors, true).unwrap();
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 2); // the matching origin row + create, no pr action row
        assert!(lines
            .iter()
            .all(|line| line.split('\t').nth(2) != Some("pr")));
    }

    /// A bare number that only matches inside another PR's number (say `69`
    /// against `#169`) must still get its action row.
    #[test]
    fn list_shows_pr_requires_the_full_hash_prefixed_number() {
        let list = "fix\t/fix\tremote\tclean\tclean\t169\torigin/fix #169\n";
        assert!(!list_shows_pr(list, 69));
        assert!(list_shows_pr(list, 169));
    }

    /// The query box accepts anything, so a typo used to reach `git worktree
    /// add` and fail with a bare refname error.
    #[test]
    fn branch_names_git_would_reject_are_caught_before_the_worktree_is_created() {
        let repo = std::env::temp_dir().join(format!("hwt-refname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.to_string_lossy().into_owned();
        assert!(crate::git::git_success(&["-C", &repo, "init", "--quiet"]));

        assert!(crate::create::valid_branch_name(&repo, "feature/x"));
        assert!(crate::create::valid_branch_name(&repo, "kees/fix-1"));
        assert!(!crate::create::valid_branch_name(&repo, "feature x"));
        assert!(!crate::create::valid_branch_name(&repo, "feature..x"));
        assert!(!crate::create::valid_branch_name(&repo, "-leading-dash"));
        assert!(!crate::create::valid_branch_name(&repo, "trailing.lock"));
        assert!(!crate::create::valid_branch_name(&repo, ""));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn pr_row_is_prepended_when_enabled_and_omitted_when_disabled() {
        if !fzf_available() {
            eprintln!("skipping real fzf regression: fzf is unavailable");
            return;
        }
        let colors = crate::theme::ThemeColors::default();
        let cached = "main\t/repo\tworktree\tclean\tclean\t\tmain\n";

        let enabled = render_query_aware_list(cached, "123", &colors, true).unwrap();
        let lines: Vec<_> = enabled.lines().collect();
        assert_eq!(lines[0].split('\t').nth(2), Some("pr"));
        assert_eq!(lines[0].split('\t').next(), Some("123"));
        assert!(lines[0].contains("checkout pull request"));
        assert_eq!(lines[1].split('\t').nth(2), Some("create"));

        let disabled = render_query_aware_list(cached, "123", &colors, false).unwrap();
        let lines: Vec<_> = disabled.lines().collect();
        assert_eq!(lines.len(), 1); // create row only, no PR row
        assert!(lines
            .iter()
            .all(|line| line.split('\t').nth(2) != Some("pr")));
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

        add_remote_tracking_worktree(
            repo.to_str().unwrap(),
            worktree.to_str().unwrap(),
            "feature",
            "origin/feature",
        )
        .unwrap();
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
    #[test]
    fn dry_run_delete_action_stops_before_inspection_or_worker_launch() {
        let config = crate::config::Config::default();
        let context = super::PickerContext {
            config: &config,
            repo: "/does-not-exist",
            dry_run: true,
        };
        let row = super::PickerRow::parse_selection("topic\t/also-does-not-exist\tworktree");
        // A real dispatch would fail while inspecting this path (and could
        // launch mutation commands for a real path). The action boundary exits.
        assert!(matches!(
            super::delete_selected(Some(row), &context).unwrap(),
            super::AfterAction::Redraw
        ));
    }

    #[test]
    fn pr_numbers_are_structured_not_prefixes_or_branch_display_markers() {
        let row = super::PickerRow {
            branch: "feature-#16",
            path: "/wt",
            entry_kind: "worktree",
            sync_kind: "",
            changes: "",
            pr_number: Some(169),
            display: "feature-#16  #169",
        };
        let list = row.to_line();
        assert!(!list_shows_pr(&list, 16));
        assert!(!list_shows_pr(&list, 69));
        assert!(list_shows_pr(&list, 169));
        let fake = super::PickerRow {
            pr_number: None,
            display: "#16",
            ..row
        };
        assert!(!list_shows_pr(&fake.to_line(), 16));
    }

    #[test]
    fn fork_fast_path_requires_repository_provenance_and_exact_head() {
        let root = std::env::temp_dir().join(format!("herdr-pr-identity-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q", "-b", "feature"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let mut target = crate::pr::PullRequestTarget {
            number: 16,
            head_ref: "feature".into(),
            head_oid: head,
            base_ref: "main".into(),
            is_cross_repo: true,
            head_repository: "fork/repo".into(),
        };
        assert!(
            super::verify_pr_checkout_identity(&target, "feature", root.to_str().unwrap()).is_err()
        );
        git(&["config", "branch.feature.herdr-pr-repository", "other/repo"]);
        assert!(
            super::verify_pr_checkout_identity(&target, "feature", root.to_str().unwrap()).is_err()
        );
        git(&["config", "branch.feature.herdr-pr-repository", "fork/repo"]);
        assert!(
            super::verify_pr_checkout_identity(&target, "feature", root.to_str().unwrap()).is_ok()
        );
        target.head_oid = "different".into();
        assert!(
            super::verify_pr_checkout_identity(&target, "feature", root.to_str().unwrap()).is_err()
        );
        target.is_cross_repo = false;
        assert!(
            super::verify_pr_checkout_identity(&target, "feature", root.to_str().unwrap()).is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fork_and_remote_checkout_helpers_use_rooted_relative_templates_and_preserve_identity() {
        let root = std::env::temp_dir().join(format!("herdr-pr-checkout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("origin")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let origin = root.join("origin");
        let local = root.join("local");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&origin, &["init", "-q", "-b", "main"]);
        git(&origin, &["commit", "--allow-empty", "-qm", "base"]);
        git(&origin, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(origin.join("fork-file"), "fork code").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-qm", "fork"]);
        let oid = git(&origin, &["rev-parse", "HEAD"]);
        git(&origin, &["update-ref", "refs/pull/16/head", &oid]);
        git(&origin, &["checkout", "-q", "main"]);
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        let config: crate::config::Config =
            toml::from_str("worktree-path = '.worktrees/{{ branch }}'").unwrap();
        let repo = local.to_str().unwrap();
        let remote_path = config.render_worktree_path(
            "remote-feature",
            "remote-feature",
            "origin/feature",
            repo,
            "user",
        );
        assert_eq!(
            remote_path,
            local.join(".worktrees/remote-feature").to_str().unwrap()
        );
        super::add_remote_tracking_worktree(repo, &remote_path, "remote-feature", "origin/feature")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&remote_path).join("fork-file")).unwrap(),
            "fork code"
        );
        let path = config.render_worktree_path("feature", "feature", "origin/main", repo, "user");
        let target = crate::pr::PullRequestTarget {
            number: 16,
            head_ref: "feature".into(),
            head_oid: oid.clone(),
            base_ref: "main".into(),
            is_cross_repo: true,
            head_repository: "fork/repo".into(),
        };
        // Same-name existing branch must be rejected even at an identical OID.
        git(&local, &["branch", "feature", &oid]);
        assert!(super::checkout_fork_pr(&target, 16, &path, "feature", repo).is_err());
        assert!(!std::path::Path::new(&path).exists());
        git(&local, &["branch", "-D", "feature"]);
        assert!(super::checkout_fork_pr(&target, 16, &path, "feature", repo).unwrap());
        assert_eq!(
            crate::pr::fork_repository(repo, "feature").as_deref(),
            Some("fork/repo")
        );
        assert!(super::verify_pr_checkout_identity(&target, "feature", repo).is_ok());
        assert_eq!(
            git(std::path::Path::new(&path), &["rev-parse", "HEAD"]),
            oid
        );
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&path).join("fork-file")).unwrap(),
            "fork code"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
