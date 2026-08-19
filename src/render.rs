//! Column layout and display rendering for the fzf list.

use crate::model::Worktree;
use std::path::Path;

pub const COL_BRANCH: usize = 32;
pub const COL_WORKTREE: usize = 24;
pub const COL_PR: usize = 8;
pub const COL_REVIEW: usize = 8;
pub const COL_THREADS: usize = 8;
pub const COL_CONFLICT: usize = 8;
pub const COL_WHEN: usize = 6;
pub const COL_CHANGES: usize = 10;
pub const COL_SYNC: usize = 10;

/// Truncate/pad a plain (ANSI-free) string to exactly `w` characters.
pub fn pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len > w {
        s.chars().take(w).collect()
    } else {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(w - len));
        out
    }
}

/// Truncate (no padding) a plain string to `w` characters.
pub fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() > w {
        s.chars().take(w).collect()
    } else {
        s.to_string()
    }
}

/// Branch name with a dimmed prefix, truncated to `width` characters.
pub fn branch_cell(branch: &str, width: usize, prefix: &str) -> String {
    if !prefix.is_empty() && branch.starts_with(prefix) {
        let short = &branch[prefix.len()..];
        let plen = prefix.chars().count();
        let mut short_chars: Vec<char> = short.chars().collect();
        if plen + short_chars.len() > width {
            short_chars.truncate(width.saturating_sub(plen));
        }
        let used = plen + short_chars.len();
        let short_str: String = short_chars.into_iter().collect();
        let mut out = format!("\x1b[2m{}\x1b[0m{}", prefix, short_str);
        out.push_str(&" ".repeat(width.saturating_sub(used)));
        out
    } else {
        let cell = trunc(branch, width);
        let used = cell.chars().count();
        let mut out = cell;
        out.push_str(&" ".repeat(width.saturating_sub(used)));
        out
    }
}

pub fn sync_colored(s: &str, kind: &str) -> String {
    let color = match kind {
        "merged" => "32",
        "ahead" => "33",
        "behind" | "gone" => "31",
        "diverged" => "35",
        _ => "2",
    };
    format!("\x1b[{color}m{s}\x1b[0m")
}

/// The final path component used as the worktree's display name.
pub fn worktree_name(path: &str) -> String {
    if path.is_empty() {
        return "—".to_string();
    }
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string())
}

/// One display line: branch, optional worktree name, PR, review, unresolved threads, conflict, when, changes, sync.
pub fn render_row(branch_disp: &str, wt: &Worktree, prefix: &str) -> String {
    render_row_with_options(branch_disp, wt, prefix, true)
}

pub fn render_row_with_options(
    branch_disp: &str,
    wt: &Worktree,
    prefix: &str,
    show_worktree_name: bool,
) -> String {
    let branch = branch_cell(branch_disp, COL_BRANCH, prefix);
    render_cells(branch, wt, show_worktree_name)
}

pub fn render_picker_row(branch_disp: &str, wt: &Worktree, prefix: &str) -> String {
    render_picker_row_with_options(branch_disp, wt, prefix, true)
}

pub fn render_picker_row_with_options(
    branch_disp: &str,
    wt: &Worktree,
    prefix: &str,
    show_worktree_name: bool,
) -> String {
    let branch = branch_cell(branch_disp, COL_BRANCH, prefix);
    render_cells(branch, wt, show_worktree_name)
}

pub fn render_picker_header() -> String {
    render_picker_header_with_options(true)
}

pub fn render_picker_header_with_options(show_worktree_name: bool) -> String {
    render_header_with_options(show_worktree_name)
}

fn render_cells(branch: String, wt: &Worktree, show_worktree_name: bool) -> String {
    let worktree = show_worktree_name.then(|| pad(&worktree_name(&wt.path), COL_WORKTREE));
    let pr = pr_cell(wt.pr_number, wt.pr_url.as_deref(), COL_PR);
    let review = review_cell(&wt.review, COL_REVIEW);
    let threads = threads_cell(&wt.threads, COL_THREADS);
    let conflict = conflict_cell(wt.conflict, COL_CONFLICT);
    let when = pad(&wt.when, COL_WHEN);
    let changes = pad(&wt.changes, COL_CHANGES);
    let sync = sync_colored(&trunc(&wt.sync, COL_SYNC), &wt.sync_kind);
    match worktree {
        Some(worktree) => {
            format!("{branch}  {worktree}  {pr}  {review}  {threads}  {conflict}  {when}  {changes}  {sync}")
        }
        None => {
            format!("{branch}  {pr}  {review}  {threads}  {conflict}  {when}  {changes}  {sync}")
        }
    }
}

/// PR number as an OSC 8 clickable link (opens on ctrl-click).
fn pr_cell(number: Option<u32>, url: Option<&str>, width: usize) -> String {
    match number {
        Some(n) => {
            let text = trunc(&format!("#{n}"), width);
            let len = text.chars().count();
            let mut cell = match url {
                Some(u) => format!("\x1b]8;;{u}\x1b\\{text}\x1b]8;;\x1b\\"),
                None => text,
            };
            cell.push_str(&" ".repeat(width.saturating_sub(len)));
            cell
        }
        None => pad("—", width),
    }
}

fn review_cell(review: &str, width: usize) -> String {
    let color = match review {
        "approved" => "32",
        "changes" => "31",
        "review" => "33",
        _ => "2", // "draft" and "—" render dim
    };
    let plain = trunc(review, width);
    let len = plain.chars().count();
    let mut cell = format!("\x1b[{color}m{plain}\x1b[0m");
    cell.push_str(&" ".repeat(width.saturating_sub(len)));
    cell
}

fn threads_cell(threads: &str, width: usize) -> String {
    let color = match threads {
        "0" => "32",
        "?" | "—" => "2",
        _ => "31",
    };
    let plain = trunc(threads, width);
    let len = plain.chars().count();
    let mut cell = format!("\x1b[{color}m{plain}\x1b[0m");
    cell.push_str(&" ".repeat(width.saturating_sub(len)));
    cell
}

/// A red `conflict` marker only when the PR has merge conflicts; blank otherwise.
fn conflict_cell(conflict: bool, width: usize) -> String {
    if conflict {
        let mut cell = "\x1b[31mconflict\x1b[0m".to_string();
        cell.push_str(&" ".repeat(width.saturating_sub("conflict".len())));
        cell
    } else {
        " ".repeat(width)
    }
}

/// Column labels aligned to `render_row` (fzf reserves the pointer column).
pub fn render_header() -> String {
    render_header_with_options(true)
}

pub fn render_header_with_options(show_worktree_name: bool) -> String {
    let worktree = show_worktree_name.then(|| pad("worktree", COL_WORKTREE));
    match worktree {
        Some(worktree) => format!(
            "{}  {}  {}  {}  {}  {}  {}  {}  {}",
            pad("branch", COL_BRANCH),
            worktree,
            pad("pr", COL_PR),
            pad("review", COL_REVIEW),
            pad("threads", COL_THREADS),
            pad("conflict", COL_CONFLICT),
            pad("when", COL_WHEN),
            pad("changes", COL_CHANGES),
            "sync"
        ),
        None => format!(
            "{}  {}  {}  {}  {}  {}  {}  {}",
            pad("branch", COL_BRANCH),
            pad("pr", COL_PR),
            pad("review", COL_REVIEW),
            pad("threads", COL_THREADS),
            pad("conflict", COL_CONFLICT),
            pad("when", COL_WHEN),
            pad("changes", COL_CHANGES),
            "sync"
        ),
    }
}

/// Compact age: now, 5m, 2h, 1d, 3w, 2mo, 1y.
pub fn relative_age(ts: i64) -> String {
    if ts <= 0 {
        return "—".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - ts;
    if delta < 60 {
        "now".to_string()
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86400 {
        format!("{}h", delta / 3600)
    } else if delta < 604800 {
        format!("{}d", delta / 86400)
    } else if delta < 2629800 {
        format!("{}w", delta / 604800)
    } else if delta < 31557600 {
        format!("{}mo", delta / 2629800)
    } else {
        format!("{}y", delta / 31557600)
    }
}
