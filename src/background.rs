//! Spawn fully-detached background work so a popup can close immediately.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Logs older than this are deleted the next time a log path is allocated.
const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Spawn `program args…` in its own process group with stdio redirected to
/// `log_path`. The child survives the plugin exiting (it is reparented to
/// init) and is not killed by SIGHUP when the popup pane closes. `WT_LOG` is
/// set so the child can reference its log in notifications. Returns the child's
/// pid, which a follower can use to tell "still working" from "died".
pub fn spawn_detached(program: &str, args: &[String], log_path: &str) -> std::io::Result<u32> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_err = log.try_clone()?;
    let child = Command::new(program)
        .args(args)
        // New process group: the child is not in the popup's process group, so
        // a SIGHUP sent when the pane closes won't reach it.
        .process_group(0)
        .env("WT_LOG", log_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()?;
    Ok(child.id())
}

/// A fresh log path under the plugin state dir, e.g.
/// `<state>/logs/setup-4821-1712345678901234567-0.log`.
pub fn log_path(kind: &str) -> String {
    let dir = crate::model::state_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    prune_logs(&dir, LOG_RETENTION);
    log_path_in(&dir, kind)
}

/// Allocate an unused log name in `dir`. The pid and nanosecond timestamp keep
/// two operations started in the same second from sharing one file — sharing
/// would truncate the running one's log and cross-terminate progress panes.
fn log_path_in(dir: &Path, kind: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut path = PathBuf::new();
    for attempt in 0..100u32 {
        path = dir.join(format!("{kind}-{pid}-{nanos}-{attempt}.log"));
        if !path.exists() {
            break;
        }
    }
    path.to_string_lossy().into_owned()
}

/// Opportunistically drop stale logs; without this the state dir grows by one
/// file per operation forever.
fn prune_logs(dir: &Path, retention: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "log") {
            continue;
        }
        let modified = entry.metadata().and_then(|metadata| metadata.modified());
        if modified.is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age > retention)
        }) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{log_path_in, prune_logs};
    use std::time::Duration;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-background-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn concurrent_removals_never_share_a_log_file() {
        let dir = temp_dir("unique-log");
        let first = log_path_in(&dir, "remove");
        std::fs::write(&first, b"first").unwrap();
        let second = log_path_in(&dir, "remove");
        assert_ne!(first, second);
        assert!(first.contains(&format!("remove-{}-", std::process::id())));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stale_logs_are_pruned_and_other_files_are_left_alone() {
        let dir = temp_dir("prune-log");
        let log = dir.join("remove-1-2-0.log");
        let keep = dir.join("not-a-log.json");
        std::fs::write(&log, b"old").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::thread::sleep(Duration::from_millis(5));

        prune_logs(&dir, Duration::from_secs(7 * 24 * 60 * 60));
        assert!(log.exists(), "a fresh log must survive");

        prune_logs(&dir, Duration::ZERO);
        assert!(!log.exists());
        assert!(keep.exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
