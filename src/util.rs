//! Small helpers shared across the plugin.

use serde::Serialize;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace characters that are unsafe in a path with `-`.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Expand `{{ name }}` and `{{ name | sanitize }}` template variables.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, val) in vars {
        out = out.replace(&format!("{{{{ {name} }}}}"), val);
        out = out.replace(&format!("{{{{ {name} | sanitize }}}}"), &sanitize(val));
    }
    out
}

/// Drop `refs/heads/`, `refs/remotes/`, then `origin/` prefixes (each at most once).
pub fn strip_remote(r: &str) -> String {
    let mut r = r;
    for p in ["refs/heads/", "refs/remotes/", "origin/"] {
        r = r.strip_prefix(p).unwrap_or(r);
    }
    r.to_string()
}

/// Collapse `.` and `x/..` segments lexically, so a template pointing at a
/// sibling directory renders the same path git records for the worktree.
pub fn normalize_path(path: &str) -> String {
    let has_relative = path.split('/').any(|s| s == "." || s == "..");
    if !has_relative {
        return path.to_string();
    }
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                if matches!(parts.last(), Some(&last) if last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(segment),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Single-quote a string for safe embedding in a POSIX shell command.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// This binary's path, for the helper commands it hands to fzf and Herdr.
/// Falls back to the bare name so a `PATH` lookup can still find it.
pub fn self_exe() -> String {
    std::env::current_exe().map_or_else(
        |_| "herdr-worktrees".to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path`, replacing it atomically: the payload goes to a
/// process-unique temp file next to the target and is renamed over it, so a
/// concurrent reader never sees a half-written file. Missing parent directories
/// are created; a failed write leaves no temp file behind.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other("path has no parent directory"));
    };
    std::fs::create_dir_all(dir)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes));
    match written {
        Ok(()) => std::fs::rename(&temp, path),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Serialize `value` as JSON and write it with [`write_atomic`].
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_string(value).map_err(std::io::Error::other)?;
    write_atomic(path, json.as_bytes())
}

/// Map `compute` over `items` on at most `workers` scoped threads, preserving
/// order. Callers pick `workers` themselves because the right number depends on
/// the work: the picker's per-entry `git` calls are latency-bound and happily
/// oversubscribe the cores, while the removal picker keeps its safety checks to
/// a handful of processes.
///
/// A chunk whose thread panics yields `None` for each of its items instead of
/// taking the whole batch down.
pub fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    compute: impl Fn(&T) -> R + Sync,
) -> Vec<Option<R>> {
    if items.is_empty() {
        return Vec::new();
    }
    let workers = workers.clamp(1, items.len());
    let chunk_size = items.len().div_ceil(workers);
    let compute = &compute;
    std::thread::scope(|scope| {
        items
            .chunks(chunk_size)
            .map(|chunk| {
                (
                    chunk.len(),
                    scope.spawn(move || chunk.iter().map(compute).map(Some).collect::<Vec<_>>()),
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|(len, handle)| {
                handle
                    .join()
                    .unwrap_or_else(|_| (0..len).map(|_| None).collect())
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::{parallel_map, self_exe, write_atomic, write_json_atomic};

    #[test]
    fn parallel_map_preserves_order_and_survives_a_panicking_item() {
        let items: Vec<u32> = (0..17).collect();
        let doubled = parallel_map(&items, 4, |item| item * 2);
        assert_eq!(
            doubled,
            items.iter().map(|item| Some(item * 2)).collect::<Vec<_>>()
        );
        assert!(parallel_map::<u32, u32>(&[], 4, |_| unreachable!()).is_empty());

        // One worker per item, so only the panicking item loses its result.
        let guarded = parallel_map(&items, items.len(), |item| {
            assert_ne!(*item, 7, "deliberate test panic");
            *item
        });
        assert_eq!(guarded[7], None);
        assert_eq!(guarded[6], Some(6));
        assert_eq!(guarded[8], Some(8));
    }


    #[test]
    fn normalizes_relative_segments() {
        let cases = [
            ("/home/dev/app/../app.fix", "/home/dev/app.fix"),
            ("/home/dev/./app", "/home/dev/app"),
            ("/home/dev/app", "/home/dev/app"),
            ("../sibling", "../sibling"),
        ];
        for (input, want) in cases {
            assert_eq!(super::normalize_path(input), want, "{input}");
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-util-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn json_write_creates_directories_and_leaves_no_temp_file() {
        let dir = temp_dir("write");
        let file = dir.join("nested").join("value.json");
        write_json_atomic(&file, &serde_json::json!({"ts": 1})).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), r#"{"ts":1}"#);
        let leftovers: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("value.json")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_write_replaces_an_existing_file() {
        let dir = temp_dir("replace");
        let file = dir.join("value.json");
        write_json_atomic(&file, &serde_json::json!({"ts": 1})).unwrap();
        write_json_atomic(&file, &serde_json::json!({"ts": 2})).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), r#"{"ts":2}"#);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn raw_write_replaces_bytes_and_leaves_no_temp_file() {
        let dir = temp_dir("bytes");
        let file = dir.join("rows").join("list.cache");
        write_atomic(&file, b"old\n").unwrap();
        write_atomic(&file, b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
        let leftovers = std::fs::read_dir(file.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn self_exe_names_the_running_binary() {
        let exe = self_exe();
        assert!(!exe.is_empty());
        assert_eq!(
            exe,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
    }
}
