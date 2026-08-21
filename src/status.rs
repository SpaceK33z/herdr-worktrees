//! Working-tree and push/pull status: how a branch stands against its
//! remote-tracking ref, and what `git status --porcelain` says about its files.

use crate::git;
use serde::Serialize;

/// The sync column's state. The wire name (`as_str`) is what the picker row
/// protocol carries and what `--json` serializes, so it must stay stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncKind {
    /// Level with the upstream.
    Synced,
    Ahead,
    Behind,
    Diverged,
    /// No upstream configured.
    Local,
    /// The configured upstream no longer exists.
    Gone,
    /// The pull request for this head was merged.
    Merged,
    /// A detached checkout, which has no branch to compare.
    Detached,
    /// A remote-tracking branch offered as a checkout candidate.
    Remote,
    /// The exact counts have not been computed yet.
    Loading,
}

impl SyncKind {
    /// The wire name used by the picker row protocol and `--json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Ahead => "ahead",
            Self::Behind => "behind",
            Self::Diverged => "diverged",
            Self::Local => "local",
            Self::Gone => "gone",
            Self::Merged => "merged",
            Self::Detached => "detached",
            Self::Remote => "remote",
            Self::Loading => "loading",
        }
    }

    /// Parse a wire name back into a kind.
    pub fn from_wire(value: &str) -> Option<Self> {
        let kind = match value {
            "synced" => Self::Synced,
            "ahead" => Self::Ahead,
            "behind" => Self::Behind,
            "diverged" => Self::Diverged,
            "local" => Self::Local,
            "gone" => Self::Gone,
            "merged" => Self::Merged,
            "detached" => Self::Detached,
            "remote" => Self::Remote,
            "loading" => Self::Loading,
            _ => return None,
        };
        Some(kind)
    }
}

/// A sync state together with the text shown in the sync column. The two travel
/// as one value so a caller cannot pair the display of one state with the kind
/// of another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub kind: SyncKind,
    pub display: String,
}

impl SyncStatus {
    pub fn new(kind: SyncKind, display: impl Into<String>) -> Self {
        Self {
            kind,
            display: display.into(),
        }
    }

    /// A status that displays its own name, as `local`, `gone`, `merged` and
    /// `remote` do.
    pub fn named(kind: SyncKind) -> Self {
        Self::new(kind, kind.as_str())
    }
}

/// Compute push/pull state against a branch's remote-tracking ref.
/// A missing upstream is a named state rather than a misleading comparison
/// with the repository's base branch.
pub fn compute_sync(branch: &str, upstream: Option<&str>, repo: &str) -> SyncStatus {
    let Some(upstream) = upstream else {
        return local();
    };
    if !git::ref_exists(repo, upstream) {
        return gone();
    }
    compute_sync_existing(branch, upstream, repo)
}

/// Compute sync when the caller already knows that `upstream` exists.
/// This avoids a redundant `rev-parse` in the batched picker path.
pub fn compute_sync_existing(branch: &str, upstream: &str, repo: &str) -> SyncStatus {
    let (behind, ahead) = left_right_count(repo, upstream, branch);
    from_counts(behind, ahead)
}

/// Parse `%(upstream:track,nobracket)` from `git for-each-ref`.
/// Git is invoked with `LC_ALL=C`, so these machine-read labels are stable.
pub fn sync_from_tracking(track: &str) -> Option<SyncStatus> {
    if track == "gone" {
        return Some(gone());
    }
    if track.is_empty() {
        return Some(from_counts(0, 0));
    }

    let mut ahead = 0;
    let mut behind = 0;
    for part in track.split(',').map(str::trim) {
        if let Some(value) = part.strip_prefix("ahead ") {
            ahead = value.parse().ok()?;
        } else {
            let value = part.strip_prefix("behind ")?;
            behind = value.parse().ok()?;
        }
    }
    Some(from_counts(behind, ahead))
}

pub fn local() -> SyncStatus {
    SyncStatus::named(SyncKind::Local)
}

fn gone() -> SyncStatus {
    SyncStatus::named(SyncKind::Gone)
}

fn from_counts(behind: u32, ahead: u32) -> SyncStatus {
    match (behind, ahead) {
        (0, 0) => SyncStatus::new(SyncKind::Synced, "—"),
        (0, ahead) => SyncStatus::new(SyncKind::Ahead, format!("↑{ahead}")),
        (behind, 0) => SyncStatus::new(SyncKind::Behind, format!("↓{behind}")),
        (behind, ahead) => SyncStatus::new(SyncKind::Diverged, format!("↑{ahead} ↓{behind}")),
    }
}

fn left_right_count(repo: &str, upstream: &str, branch: &str) -> (u32, u32) {
    let spec = format!("{upstream}...{branch}");
    let out = git::git_stdout(&["-C", repo, "rev-list", "--left-right", "--count", &spec]);
    let mut counts = out.split_whitespace();
    let behind = counts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let ahead = counts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (behind, ahead)
}

/// Working-tree change counts from one `git status --porcelain` run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

impl ChangeCounts {
    /// Whether the worktree holds any work at all — the removal path's safety
    /// question, so untracked files count.
    pub fn dirty(self) -> bool {
        self.staged > 0 || self.unstaged > 0 || self.untracked > 0
    }
}

/// Count the entries of `git status --porcelain` output.
///
/// Callers differ in how they present these counts — the picker folds untracked
/// files into the unstaged column, the removal picker shows them separately —
/// but they must agree on what counts as work, so there is one parser.
/// Unrecognized index codes count as staged rather than as nothing: a worktree
/// with changes must never render as clean, nor be removed as if it were.
pub fn parse_porcelain(output: &[u8]) -> ChangeCounts {
    let mut counts = ChangeCounts::default();
    for line in output.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let x = line[0];
        let y = line.get(1).copied().unwrap_or(b' ');
        if x == b'?' && y == b'?' {
            counts.untracked += 1;
            continue;
        }
        // Unmerged paths (`UU`, `AU`, `UD`, and the `AA`/`DD` pairs) need a
        // resolution rather than a commit, so they count once as unstaged work
        // instead of being read as a staged add or delete.
        if x == b'U' || y == b'U' || (x == y && matches!(x, b'A' | b'D')) {
            counts.unstaged += 1;
            continue;
        }
        if x != b' ' && x != b'?' {
            counts.staged += 1;
        }
        if y != b' ' && y != b'?' {
            counts.unstaged += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::{parse_porcelain, sync_from_tracking, ChangeCounts, SyncKind, SyncStatus};

    fn status(kind: SyncKind, display: &str) -> Option<SyncStatus> {
        Some(SyncStatus::new(kind, display))
    }

    #[test]
    fn parses_for_each_ref_tracking_counts() {
        assert_eq!(sync_from_tracking(""), status(SyncKind::Synced, "—"));
        assert_eq!(sync_from_tracking("ahead 3"), status(SyncKind::Ahead, "↑3"));
        assert_eq!(
            sync_from_tracking("behind 2"),
            status(SyncKind::Behind, "↓2")
        );
        assert_eq!(
            sync_from_tracking("ahead 3, behind 2"),
            status(SyncKind::Diverged, "↑3 ↓2")
        );
        assert_eq!(sync_from_tracking("gone"), status(SyncKind::Gone, "gone"));
        assert_eq!(sync_from_tracking("unexpected"), None);
    }

    /// The wire names are part of the row protocol and of `--json`.
    #[test]
    fn sync_kinds_round_trip_through_their_wire_names() {
        for kind in [
            SyncKind::Synced,
            SyncKind::Ahead,
            SyncKind::Behind,
            SyncKind::Diverged,
            SyncKind::Local,
            SyncKind::Gone,
            SyncKind::Merged,
            SyncKind::Detached,
            SyncKind::Remote,
            SyncKind::Loading,
        ] {
            assert_eq!(SyncKind::from_wire(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        assert_eq!(SyncKind::from_wire("unexpected"), None);
    }

    #[test]
    fn counts_staged_unstaged_and_untracked_porcelain_codes() {
        assert_eq!(parse_porcelain(b""), ChangeCounts::default());
        assert_eq!(
            parse_porcelain(b"M  a\n M b\n?? c\n"),
            ChangeCounts {
                staged: 1,
                unstaged: 1,
                untracked: 1,
            }
        );
        assert_eq!(
            parse_porcelain(b"MM a\n"),
            ChangeCounts {
                staged: 1,
                unstaged: 1,
                untracked: 0,
            }
        );
        assert_eq!(
            parse_porcelain(b"R  old -> new\n"),
            ChangeCounts {
                staged: 1,
                unstaged: 0,
                untracked: 0,
            }
        );
    }

    #[test]
    fn counts_unmerged_and_typechange_paths_as_work_in_progress() {
        // A conflicted worktree must never render as clean.
        for line in [
            &b"UU a\n"[..],
            b"AA a\n",
            b"DD a\n",
            b"AU a\n",
            b"UD a\n",
            b"UA a\n",
        ] {
            assert_eq!(
                parse_porcelain(line),
                ChangeCounts {
                    staged: 0,
                    unstaged: 1,
                    untracked: 0,
                },
                "unmerged line {:?}",
                String::from_utf8_lossy(line)
            );
        }
        assert_eq!(parse_porcelain(b"T  a\n").staged, 1);
        assert_eq!(parse_porcelain(b" T a\n").unstaged, 1);
        assert_eq!(parse_porcelain(b"TT a\n"), parse_porcelain(b"MM a\n"));
    }

    /// The removal path must treat anything it does not recognize as work.
    #[test]
    fn an_unrecognized_index_code_still_counts_as_a_change() {
        assert!(parse_porcelain(b"X  a\n").dirty());
        assert!(!parse_porcelain(b"").dirty());
    }
}
