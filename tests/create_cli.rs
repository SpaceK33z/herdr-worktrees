//! Exercise the actual create CLI with real git and a fake Herdr executable.
//! No test may create layout in the developer's running Herdr session.
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(mode: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-wt-create-cli-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Git resolves symlinked temp roots (e.g. /var -> /private/var on macOS).
        let dir = std::fs::canonicalize(dir).unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        for args in [
            vec!["init", "--quiet", "-b", "main"],
            vec![
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let output = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        std::fs::write(
            dir.join("config/config.toml"),
            format!(
                "open-mode = {mode:?}\nbranch-prefix = 'test/'\nbase-branch = 'main'\n\
                 worktree-path = '{{{{ repo_path }}}}/../{{{{ branch_short }}}}'\n\
                 fetch-before-create = false\nauto-detect = false\nworktree-include = false\n"
            ),
        )
        .unwrap();
        let mock = dir.join("herdr");
        std::fs::write(
            &mock,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$TEST_HERDR_CALLS"
case "$1 $2" in
  'worktree list')
    if [ "$TEST_ATTACHMENT" = absent ]; then
      printf '%s\n' '{"result":{"source":{"source_workspace_id":null}}}'
    else
      printf '%s\n' '{"result":{"source":{"source_workspace_id":"w2"}}}'
    fi
    ;;
  'worktree open')
    [ "$TEST_ATTACHMENT" != failed ] || exit 1
    printf '%s\n' '{"result":{"source_workspace_id":"w2","root_pane":{"workspace_id":"wAF","pane_id":"wAF:p7"}}}'
    ;;
  'tab create')
    printf '%s\n' '{"result":{"root_pane":{"workspace_id":"w2","pane_id":"w2:p9"}}}'
    ;;
  *) echo "unexpected Herdr call: $*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&mock, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn create(&self, extra: &[&str], attachment: &str) -> (Value, Vec<String>) {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-worktrees"))
            .current_dir(self.dir.join("repo"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.dir.join("config"))
            .env("HERDR_BIN_PATH", self.dir.join("herdr"))
            .env("TEST_HERDR_CALLS", self.dir.join("calls"))
            .env("TEST_ATTACHMENT", attachment)
            .args(["create", "parser-fix", "--json"])
            .args(extra)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        // Parse ALL stdout: git progress must not precede the JSON document.
        let result: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("invalid JSON: {error}; output: {output:?}"));
        assert_eq!(result["branch"], "test/parser-fix");
        assert_eq!(
            result["path"],
            self.dir.join("parser-fix").to_str().unwrap()
        );
        let calls = std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        (result, calls)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn workspace_attachment_returns_destination_not_source_and_opens_once() {
    let fixture = Fixture::new("workspace");
    let (result, calls) = fixture.create(&[], "ok");
    assert_eq!(result["attachedWorkspaceId"], "wAF");
    assert_eq!(result["workspace"], "wAF");
    assert_eq!(result["rootPaneId"], "wAF:p7");
    assert_eq!(
        calls,
        vec![
            format!("worktree list --cwd {}", fixture.dir.join("repo").display()),
            format!(
                "worktree open --workspace w2 --path {} --label test/parser-fix --no-focus",
                fixture.dir.join("parser-fix").display()
            ),
        ]
    );
}

#[test]
fn tab_attachment_returns_the_new_tab_pane_in_the_source_workspace() {
    let fixture = Fixture::new("tab");
    let (result, calls) = fixture.create(&[], "ok");
    assert_eq!(result["attachedWorkspaceId"], "w2");
    assert_eq!(result["workspace"], "w2");
    assert_eq!(result["rootPaneId"], "w2:p9");
    assert_eq!(
        calls,
        vec![
            format!("worktree list --cwd {}", fixture.dir.join("repo").display()),
            format!(
                "tab create --workspace w2 --cwd {} --label test/parser-fix --no-focus",
                fixture.dir.join("parser-fix").display()
            ),
        ]
    );
}

#[test]
fn setup_output_does_not_contaminate_json_and_setup_finishes_before_return() {
    use std::io::Write;

    let fixture = Fixture::new("workspace");
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.dir.join("config/config.toml"))
        .unwrap();
    writeln!(config, "\n[pre-start]\nsetup-worktree = '''\necho setup-output\necho setup-error >&2\nprintf ready > prepared\n'''" ).unwrap();
    let (result, _) = fixture.create(&[], "ok");
    assert_eq!(result["setup"], "setup complete");
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("parser-fix/prepared")).unwrap(),
        "ready"
    );
}

#[test]
fn no_open_skips_herdr_entirely() {
    let fixture = Fixture::new("workspace");
    let (result, calls) = fixture.create(&["--no-open"], "ok");
    assert_no_attachment(&result);
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn missing_source_workspace_returns_null_ids_without_creating_layout() {
    let fixture = Fixture::new("workspace");
    let (result, calls) = fixture.create(&[], "absent");
    assert_no_attachment(&result);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("worktree list "));
}

#[test]
fn failed_attachment_does_not_report_source_ids_or_retry_with_generic_workspace() {
    let fixture = Fixture::new("workspace");
    let (result, calls) = fixture.create(&[], "failed");
    assert_no_attachment(&result);
    assert_eq!(calls.len(), 2);
    assert!(calls[1].starts_with("worktree open "));
}

fn assert_no_attachment(result: &Value) {
    for key in ["workspace", "attachedWorkspaceId", "rootPaneId"] {
        assert_eq!(result.get(key), Some(&Value::Null), "{key}: {result}");
    }
}
