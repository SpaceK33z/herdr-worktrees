//! Push/pull status for a local branch relative to its remote-tracking ref.

use crate::git;

/// Compute push/pull state against a branch's remote-tracking ref.
/// A missing upstream is a named state rather than a misleading comparison
/// with the repository's base branch.
pub fn compute_sync(branch: &str, upstream: Option<&str>, repo: &str) -> (String, String) {
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
pub fn compute_sync_existing(branch: &str, upstream: &str, repo: &str) -> (String, String) {
    let (behind, ahead) = left_right_count(repo, upstream, branch);
    from_counts(behind, ahead)
}

/// Parse `%(upstream:track,nobracket)` from `git for-each-ref`.
/// Git is invoked with `LC_ALL=C`, so these machine-read labels are stable.
pub fn sync_from_tracking(track: &str) -> Option<(String, String)> {
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
        } else if let Some(value) = part.strip_prefix("behind ") {
            behind = value.parse().ok()?;
        } else {
            return None;
        }
    }
    Some(from_counts(behind, ahead))
}

pub fn local() -> (String, String) {
    ("local".to_string(), "local".to_string())
}

fn gone() -> (String, String) {
    ("gone".to_string(), "gone".to_string())
}

fn from_counts(behind: u32, ahead: u32) -> (String, String) {
    match (behind, ahead) {
        (0, 0) => ("synced".to_string(), "—".to_string()),
        (0, ahead) => ("ahead".to_string(), format!("↑{ahead}")),
        (behind, 0) => ("behind".to_string(), format!("↓{behind}")),
        (behind, ahead) => ("diverged".to_string(), format!("↑{ahead} ↓{behind}")),
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

#[cfg(test)]
mod tests {
    use super::sync_from_tracking;

    #[test]
    fn parses_for_each_ref_tracking_counts() {
        assert_eq!(sync_from_tracking(""), Some(("synced".into(), "—".into())));
        assert_eq!(
            sync_from_tracking("ahead 3"),
            Some(("ahead".into(), "↑3".into()))
        );
        assert_eq!(
            sync_from_tracking("behind 2"),
            Some(("behind".into(), "↓2".into()))
        );
        assert_eq!(
            sync_from_tracking("ahead 3, behind 2"),
            Some(("diverged".into(), "↑3 ↓2".into()))
        );
        assert_eq!(
            sync_from_tracking("gone"),
            Some(("gone".into(), "gone".into()))
        );
        assert_eq!(sync_from_tracking("unexpected"), None);
    }
}
