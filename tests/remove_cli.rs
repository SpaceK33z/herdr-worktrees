//! Real Git safety and interleaving tests; only disposable repositories and a
//! disabled Herdr executable. The wrapper injects failures at subprocess seams.
#![cfg(unix)]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
    git: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        // The clock alone is not unique: macOS reports `as_nanos` at only
        // microsecond granularity, so two fixtures built in the same tick on
        // different test threads would share a directory and race each other's
        // `git init` over the template files. The counter keeps them apart.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!(
            "herdr-remove-regression-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("repo")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let git = PathBuf::from(
            String::from_utf8(
                Command::new("sh")
                    .args(["-c", "command -v git"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        );
        let fixture = Self {
            repo: root.join("repo"),
            worktree: root.join("feature"),
            root,
            git,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.invalid"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        fs::write(fixture.repo.join("f"), "base\n").unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "base"]);
        fixture.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            fixture.worktree.to_str().unwrap(),
        ]);
        fs::create_dir(fixture.root.join("config")).unwrap();
        fs::write(
            fixture.root.join("config/config.toml"),
            "auto-detect = false\nbase-branch = 'main'\n[remove]\ndelete-branch = true\n",
        )
        .unwrap();
        fs::create_dir(fixture.root.join("bin")).unwrap();
        fixture
    }
    fn git(&self, args: &[&str]) -> String {
        self.git_at(&self.repo, args)
    }
    fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new(&self.git)
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
    fn unpublished(&self) {
        fs::write(self.worktree.join("f"), "unpublished\n").unwrap();
        self.git_at(&self.worktree, &["commit", "-qam", "unpublished"]);
    }
    fn wrapper(&self, script: &str) {
        let file = self.root.join("bin/git");
        fs::write(
            &file,
            format!("#!/bin/sh\nset -eu\n{script}\nexec \"$REAL_GIT\" \"$@\"\n"),
        )
        .unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn remove(&self, mode: &str) -> Output {
        let head = self.git(&["rev-parse", "refs/heads/feature"]);
        self.run(mode)
            .args([
                "remove-bg-batch",
                self.repo.to_str().unwrap(),
                "feature",
                self.worktree.to_str().unwrap(),
                &head,
                "safe",
            ])
            .output()
            .unwrap()
    }
    /// The `remove --target` entry point, with stdin closed like a script's.
    fn remove_target(&self, flags: &[&str]) -> Output {
        self.run("")
            .args([
                "remove",
                "--target",
                "feature",
                self.worktree.to_str().unwrap(),
            ])
            .args(flags)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
    /// Poll until the detached removal worker has finished (or given up).
    fn wait_for_removal(&self) -> String {
        let logs = self.root.join("state/logs");
        for _ in 0..200 {
            if let Ok(entries) = fs::read_dir(&logs) {
                for entry in entries.flatten() {
                    let log = fs::read_to_string(entry.path()).unwrap_or_default();
                    if log.contains("__HERDR_WORKTREE_REMOVE_DONE__") {
                        return log;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("removal worker never finished");
    }
    fn run(&self, mode: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-worktrees"));
        command
            .current_dir(&self.repo)
            .env("HERDR_BIN_PATH", "/bin/true")
            .env("HERDR_PLUGIN_CONFIG_DIR", self.root.join("config"))
            .env("HERDR_PLUGIN_STATE_DIR", self.root.join("state"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("REAL_GIT", &self.git)
            .env("FIXTURE", &self.root)
            .env("MODE", mode);
        command
    }
    fn branch_exists(&self) -> bool {
        Command::new(&self.git)
            .arg("-C")
            .arg(&self.repo)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/feature"])
            .status()
            .unwrap()
            .success()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn same_named_tag_cannot_authorize_unpublished_branch_deletion() {
    let f = Fixture::new();
    f.git(&["update-ref", "refs/remotes/origin/feature", "HEAD"]);
    f.git(&["tag", "feature"]);
    f.unpublished();
    let out = f.remove("");
    assert!(f.worktree.exists(), "{out:?}");
    assert!(f.branch_exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("safety state changed"));
}

#[test]
fn git_diff_failure_fails_closed_even_with_empty_stdout() {
    let f = Fixture::new();
    f.unpublished();
    f.wrapper("case \" $* \" in *' diff --name-only '*) exit 128;; esac");
    let out = f.remove("");
    assert!(f.worktree.exists(), "{out:?}");
    assert!(f.branch_exists());
}

#[test]
fn reservation_blocks_ordinary_sibling_checkout_through_unregister_and_ref_deletion() {
    let f = Fixture::new();
    let sibling = f.root.join("sibling");
    f.git(&[
        "worktree",
        "add",
        "--detach",
        "-q",
        sibling.to_str().unwrap(),
        "main",
    ]);
    f.wrapper(r#"
case " $* " in
  *" worktree remove $FIXTURE/feature ")
    "$REAL_GIT" "$@"
    if "$REAL_GIT" -C "$FIXTURE/sibling" switch feature >"$FIXTURE/switch.log" 2>&1; then
      echo unsafe >"$FIXTURE/occupancy"
    elif "$REAL_GIT" -C "$FIXTURE/repo" worktree add "$FIXTURE/intruder" feature >"$FIXTURE/add.log" 2>&1; then
      echo unsafe >"$FIXTURE/occupancy"
    else
      echo protected >"$FIXTURE/occupancy"
    fi
    exit 0;;
esac
"#);
    let out = f.remove("");
    assert_eq!(
        fs::read_to_string(f.root.join("occupancy")).unwrap().trim(),
        "protected",
        "{out:?}"
    );
    assert!(!f.worktree.exists());
    assert!(!f.branch_exists(), "{out:?}");
    assert_eq!(
        f.git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        2
    );
    let log = fs::read_to_string(f.root.join("switch.log")).unwrap();
    assert!(
        log.contains("already checked out") || log.contains("already used by worktree"),
        "{log}"
    );
}

#[test]
fn integration_target_reset_or_deletion_after_removal_keeps_branch() {
    for mode in ["reset", "delete"] {
        let f = Fixture::new();
        f.wrapper(
            r#"
case " $* " in
  *" worktree remove $FIXTURE/feature ")
    "$REAL_GIT" "$@"
    if [ "$MODE" = delete ]; then
      "$REAL_GIT" -C "$FIXTURE/repo" update-ref -d refs/heads/main
    else
      tree=$("$REAL_GIT" -C "$FIXTURE/repo" rev-parse 'main^{tree}')
      oid=$(printf 'changed\n' | "$REAL_GIT" -C "$FIXTURE/repo" commit-tree "$tree")
      "$REAL_GIT" -C "$FIXTURE/repo" update-ref refs/heads/main "$oid"
    fi
    exit 0;;
esac
"#,
        );
        let out = f.remove(mode);
        assert!(!f.worktree.exists(), "{out:?}");
        assert!(f.branch_exists(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("state changed"),
            "{out:?}"
        );
        assert_eq!(
            f.git(&["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
    }
}

#[test]
fn reservation_failure_preserves_original_worktree_and_branch() {
    let f = Fixture::new();
    f.wrapper("case \" $* \" in *' worktree add --detach --no-checkout '*) exit 128;; esac");
    let out = f.remove("");
    assert!(f.worktree.exists(), "{out:?}");
    assert!(f.branch_exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("could not reserve"));
}

#[test]
fn default_removal_preserves_branch_but_explicit_true_deletes_it() {
    for enabled in [false, true] {
        let f = Fixture::new();
        if !enabled {
            fs::write(f.root.join("config/config.toml"), "auto-detect = false\n").unwrap();
        }
        let out = f.remove("");
        assert!(!f.worktree.exists(), "{out:?}");
        assert_eq!(f.branch_exists(), !enabled, "{out:?}");
    }
}

#[test]
fn deletion_transaction_locks_supporting_ref_until_branch_delete_commits() {
    let f = Fixture::new();
    let hook = f.repo.join(".git/hooks/reference-transaction");
    fs::write(
        &hook,
        format!(
            r#"#!/bin/sh
[ "$1" = prepared ] || exit 0
if grep ' 0000000000000000000000000000000000000000 refs/heads/feature$' >/dev/null; then
  if "{}" -C "{}" update-ref -d refs/heads/main 2>"{}/lock-error"; then
    echo unsafe >"{}/transaction-lock"
  else
    echo protected >"{}/transaction-lock"
  fi
fi
exit 0
"#,
            f.git.display(),
            f.repo.display(),
            f.root.display(),
            f.root.display(),
            f.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let out = f.remove("");
    assert!(!f.branch_exists(), "{out:?}");
    assert_eq!(
        fs::read_to_string(f.root.join("transaction-lock"))
            .unwrap()
            .trim(),
        "protected"
    );
    assert!(fs::read_to_string(f.root.join("lock-error"))
        .unwrap()
        .contains("cannot lock ref"));
}

#[test]
fn branch_oid_and_upstream_are_pinned_through_file_removal() {
    for mode in ["branch", "upstream"] {
        let f = Fixture::new();
        f.git(&["update-ref", "refs/remotes/origin/feature", "HEAD"]);
        f.wrapper(
            r#"
case " $* " in
  *" worktree remove $FIXTURE/feature ")
    "$REAL_GIT" "$@"
    if [ "$MODE" = upstream ]; then
      "$REAL_GIT" -C "$FIXTURE/repo" update-ref -d refs/remotes/origin/feature
    else
      tree=$("$REAL_GIT" -C "$FIXTURE/repo" rev-parse 'main^{tree}')
      oid=$(printf 'changed\n' | "$REAL_GIT" -C "$FIXTURE/repo" commit-tree "$tree")
      "$REAL_GIT" -C "$FIXTURE/repo" update-ref refs/heads/feature "$oid"
    fi
    exit 0;;
esac
"#,
        );
        let out = f.remove(mode);
        assert!(!f.worktree.exists(), "{out:?}");
        assert!(f.branch_exists(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("state changed"),
            "{out:?}"
        );
    }
}

#[test]
fn changed_target_during_reservation_is_refused_without_removing_either_checkout() {
    let f = Fixture::new();
    f.git(&["branch", "other"]);
    f.wrapper(
        r#"
case " $* " in
  *' worktree add --detach --no-checkout '*)
    "$REAL_GIT" -C "$FIXTURE/feature" switch other
    "$REAL_GIT" -C "$FIXTURE/repo" worktree add "$FIXTURE/sibling" feature
    ;;
esac
"#,
    );
    let out = f.remove("");
    assert!(f.worktree.exists(), "{out:?}");
    assert!(f.root.join("sibling").exists());
    assert!(f.branch_exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("worktree changed while reserving"));
    assert_eq!(
        f.git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        3
    );
}

#[test]
fn failed_reservation_cleanup_reports_recoverable_path_and_pinned_commit() {
    let f = Fixture::new();
    f.wrapper(
        r#"
case " $* " in
  *' worktree remove --force --force '*'herdr-branch-reservation-'*) exit 128;;
esac
"#,
    );
    let out = f.remove("");
    assert!(!f.worktree.exists(), "{out:?}");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(log.contains("reservation cleanup failed"), "{log}");
    assert!(log.contains("pinned commit"));
    assert!(log.contains("update-ref --no-deref HEAD"));
    // Recover only the isolated reservation created for this fixture.
    let listing = f.git(&["worktree", "list", "--porcelain"]);
    let reservation = listing
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .find(|p| p.contains("herdr-branch-reservation-"))
        .unwrap();
    f.git(&["worktree", "remove", "--force", "--force", reservation]);
    fs::remove_dir(Path::new(reservation).parent().unwrap()).unwrap();
}

fn combined_output(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn remove_target_needs_a_terminal_without_yes() {
    let f = Fixture::new();
    let out = f.remove_target(&[]);
    let log = combined_output(&out);
    assert!(log.contains("stdin is not a terminal"), "{log}");
    assert!(f.worktree.exists());
    assert!(f.branch_exists());
}

#[test]
fn remove_target_yes_removes_a_safe_worktree_without_a_terminal() {
    let f = Fixture::new();
    let out = f.remove_target(&["--yes"]);
    let log = combined_output(&out);
    assert!(out.status.success(), "{log}");
    assert!(
        log.contains("removing 'feature' in the background"),
        "{log}"
    );
    let worker = f.wait_for_removal();
    assert!(worker.contains("Removed 'feature'"), "{worker}");
    assert!(!f.worktree.exists());
    assert!(!f.branch_exists());
}

#[test]
fn remove_target_yes_refuses_risky_worktrees_without_force() {
    let f = Fixture::new();
    f.unpublished();
    let out = f.remove_target(&["-y"]);
    let log = combined_output(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("'feature' has unpublished commits"), "{log}");
    assert!(log.contains("pass --force"), "{log}");
    assert!(f.worktree.exists());
    assert!(f.branch_exists());
}

#[test]
fn remove_target_yes_force_removes_a_risky_worktree() {
    let f = Fixture::new();
    fs::write(f.worktree.join("scratch"), "wip\n").unwrap();
    let out = f.remove_target(&["--yes", "--force"]);
    let log = combined_output(&out);
    assert!(out.status.success(), "{log}");
    let worker = f.wait_for_removal();
    assert!(worker.contains("Removed 'feature'"), "{worker}");
    assert!(!f.worktree.exists());
}

#[test]
fn remove_target_rejects_unknown_flags() {
    let f = Fixture::new();
    let out = f.remove_target(&["--now"]);
    let log = combined_output(&out);
    assert!(!out.status.success());
    assert!(log.contains("unknown argument '--now'"), "{log}");
    assert!(log.contains("usage: remove --target"), "{log}");
    assert!(f.worktree.exists());
}
