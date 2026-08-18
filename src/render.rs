//! Column layout and display rendering for the fzf list.

use crate::model::Worktree;

pub const COL_BRANCH: usize = 32;
pub const COL_PR: usize = 8;
pub const COL_REVIEW: usize = 8;
pub const COL_CONFLICT: usize = 8;
pub const COL_WHEN: usize = 6;
pub const COL_CHANGES: usize = 10;
pub const COL_STATUS: usize = 10;

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

pub fn status_colored(s: &str, kind: &str) -> String {
    let color = match kind {
        "merged" => "2",
        "squashed" => "32",
        "ahead" => "33",
        "behind" => "31",
        "diverged" => "35",
        _ => "2",
    };
    format!("\x1b[{color}m{s}\x1b[0m")
}

/// One display line: branch, PR, review, conflict, when, changes, status.
pub fn render_row(branch_disp: &str, wt: &Worktree, prefix: &str) -> String {
    let b = branch_cell(branch_disp, COL_BRANCH, prefix);
    let pr = pr_cell(wt.pr_number, wt.pr_url.as_deref(), COL_PR);
    let rv = review_cell(&wt.review, COL_REVIEW);
    let cf = conflict_cell(wt.conflict, COL_CONFLICT);
    let w = pad(&wt.when, COL_WHEN);
    let c = pad(&wt.changes, COL_CHANGES);
    let st = status_colored(&trunc(&wt.status, COL_STATUS), &wt.status_kind);
    format!("{b}  {pr}  {rv}  {cf}  {w}  {c}  {st}")
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
    format!(
        "{}  {}  {}  {}  {}  {}  {}",
        pad("branch", COL_BRANCH),
        pad("pr", COL_PR),
        pad("review", COL_REVIEW),
        pad("conflict", COL_CONFLICT),
        pad("when", COL_WHEN),
        pad("changes", COL_CHANGES),
        "status"
    )
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
