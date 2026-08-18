//! Spawn fully-detached background work so a popup can close immediately.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Spawn `program args…` in its own process group with stdio redirected to
/// `log_path`. The child survives the plugin exiting (it is reparented to
/// init) and is not killed by SIGHUP when the popup pane closes. `WT_LOG` is
/// set so the child can reference its log in notifications.
pub fn spawn_detached(program: &str, args: &[String], log_path: &str) -> std::io::Result<()> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_err = log.try_clone()?;
    Command::new(program)
        .args(args)
        // New process group: the child is not in the popup's process group, so
        // a SIGHUP sent when the pane closes won't reach it.
        .process_group(0)
        .env("WT_LOG", log_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()?;
    Ok(())
}

/// A fresh log path under the plugin state dir, e.g. `<state>/logs/setup-123.log`.
pub fn log_path(kind: &str) -> String {
    let dir = crate::model::state_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("{kind}-{epoch}.log"))
        .to_string_lossy()
        .into_owned()
}
