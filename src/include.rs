//! `.worktreeinclude`: carry gitignored files into a fresh checkout.
//!
//! A worktree starts from tracked files only, so the gitignored things a
//! project needs to actually run — `.env`, a local secrets file, a warm
//! dependency directory — are missing. Claude Code and Worktrunk read the same
//! convention for fixing that: a `.worktreeinclude` file in the repo root,
//! written in `.gitignore` syntax, naming what to carry over.
//!
//! This module follows Claude Code's reading of the convention: nothing is
//! copied unless the file exists, and an entry is copied only when it is *both*
//! named by that file and ignored by git, so tracked files are never
//! duplicated. (Worktrunk instead copies every gitignored file by default and
//! treats `.worktreeinclude` as a filter; its `--require-include` flag is the
//! behavior implemented here.)
//!
//! git owns the pattern matching, so the syntax is exactly `.gitignore`'s —
//! anchoring, `**`, and negation included. Three questions, asked of git:
//!
//! 1. What is ignored? `git status --ignored=matching` lists ignored entries and
//!    collapses a fully ignored directory (`node_modules/`) into one entry.
//! 2. What does `.worktreeinclude` name? `git ls-files --exclude-from` with
//!    `--directory` answers at directory granularity, which is what makes a
//!    whole-directory copy possible.
//! 3. For anything left over, what does it name *inside* those directories?
//!    The same call without `--directory`, limited to the leftovers, catches
//!    patterns that reach below a directory git had already collapsed.
//!
//! The third call walks the leftover directories, so a repo with a large
//! ignored `node_modules/` pays a directory traversal it would otherwise skip.
//! It runs in the background pane alongside the setup script, where a second of
//! `git ls-files` does not compete with anything.

use crate::config::Config;
use crate::git;
use std::path::{Path, PathBuf};

/// The file a repo declares its worktree extras in, read from the repo root.
pub const INCLUDE_FILE: &str = ".worktreeinclude";

/// One thing to copy, as git names it: a path relative to the repo root, with a
/// trailing `/` when git collapsed a fully ignored directory into one entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry(pub String);

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.0.ends_with('/')
    }

    /// The path without the directory marker, for joining onto a root.
    pub fn relative(&self) -> &str {
        self.0.trim_end_matches('/')
    }
}

/// What a copy did, per entry, for the caller to report.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub copied: Vec<String>,
    /// Entries already present in the destination, or holding a checkout.
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Outcome {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.skipped.is_empty() && self.failed.is_empty()
    }
}

pub fn include_file(repo: &str) -> PathBuf {
    Path::new(repo).join(INCLUDE_FILE)
}

/// Does this repo ask for `.worktreeinclude` copying at all? Checked before a
/// new worktree schedules any background work, so a repo without the file (the
/// common case) costs nothing.
pub fn applies(repo: &str, config: &Config) -> bool {
    config.worktree_include() && include_file(repo).is_file()
}

/// Everything to copy out of `repo`: ignored by git, and named by
/// `.worktreeinclude`. Empty when the file is absent.
pub fn entries(repo: &str) -> Vec<Entry> {
    let include = include_file(repo);
    if !include.is_file() {
        return Vec::new();
    }
    select(
        &ignored_entries(repo),
        &matched_directories(repo, &include),
        |leftover| matched_files(repo, &include, leftover),
    )
}

/// Intersect what git ignores with what `.worktreeinclude` names.
///
/// `matched` answers at directory granularity: an entry it names — directly, or
/// through a directory above it — is copied whole. Whatever it does not name
/// goes back to git as `deep`, which reports the individual files matched
/// inside those paths; a pattern like `packages/app/.env` is only visible that
/// way, because `--directory` stops at the untracked `packages/` above it.
///
/// Only ignored entries are ever copied, so an untracked file that
/// `.worktreeinclude` names but git tracks or does not ignore stays behind.
fn select(
    ignored: &[Entry],
    matched: &[String],
    deep: impl FnOnce(&[String]) -> Vec<String>,
) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut leftover: Vec<String> = Vec::new();
    for entry in ignored {
        if covered(matched, &entry.0) {
            out.push(entry.clone());
        } else {
            leftover.push(entry.0.clone());
        }
    }
    if !leftover.is_empty() {
        // `ls-files` without `--directory` reports files, never directories.
        out.extend(deep(&leftover).into_iter().map(Entry));
    }
    out.sort();
    out.dedup();
    out
}

/// Is `path` named by one of the collapsed matches — itself, or a directory
/// above it? The trailing `/` keeps `foo/` from claiming `foobar/x`.
fn covered(matched: &[String], path: &str) -> bool {
    matched
        .iter()
        .any(|m| m == path || (m.ends_with('/') && path.starts_with(m.as_str())))
}

/// Ignored entries, with fully ignored directories collapsed into one entry so
/// a dependency directory copies as a single (reflinked) operation.
fn ignored_entries(repo: &str) -> Vec<Entry> {
    git::git_stdout(&[
        "-C",
        repo,
        "status",
        "--porcelain",
        "-z",
        "--ignored=matching",
    ])
    .split('\0')
    .filter_map(|record| record.strip_prefix("!! "))
    .map(|path| Entry(path.to_string()))
    .collect()
}

/// What `.worktreeinclude` names, at directory granularity.
fn matched_directories(repo: &str, include: &Path) -> Vec<String> {
    ls_files(repo, include, &["--directory", "--no-empty-directory"], &[])
}

/// What `.worktreeinclude` names inside `within`, file by file.
fn matched_files(repo: &str, include: &Path, within: &[String]) -> Vec<String> {
    ls_files(repo, include, &[], within)
}

/// `git ls-files --others --ignored` against `.worktreeinclude` alone: with no
/// `--exclude-standard`, "ignored" means "matched by that file".
fn ls_files(repo: &str, include: &Path, flags: &[&str], paths: &[String]) -> Vec<String> {
    let exclude = format!("--exclude-from={}", include.display());
    let mut args = vec![
        "-C",
        repo,
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        &exclude,
    ];
    args.extend_from_slice(flags);
    if !paths.is_empty() {
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
    }
    git::git_stdout(&args)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// Copy every selected entry from `repo` into the new checkout at `dest`.
///
/// Existing destination files are left alone, so a re-run is safe, and progress
/// is printed as each entry lands — a large dependency copy otherwise looks
/// like a hung setup pane.
pub fn copy_into(repo: &str, dest: &str, config: &Config) -> Outcome {
    let mut outcome = Outcome::default();
    if !applies(repo, config) || Path::new(repo) == Path::new(dest) {
        return outcome;
    }
    let checkouts = checkouts(repo);
    for entry in entries(repo) {
        let name = entry.0.clone();
        let source = Path::new(repo).join(entry.relative());
        let target = Path::new(dest).join(entry.relative());
        // A `.worktreeinclude` broad enough to name an ignored `.worktrees/`
        // would otherwise copy other checkouts into this one.
        if holds_checkout(&source, &checkouts) {
            println!("include: skipped {name} (holds a worktree)");
            outcome.skipped.push(name);
        } else if target.symlink_metadata().is_ok() {
            println!("include: skipped {name} (already present)");
            outcome.skipped.push(name);
        } else {
            match copy_path(&source, &target) {
                Ok(()) => {
                    println!("include: copied {name}");
                    outcome.copied.push(name);
                }
                Err(err) => {
                    println!("include: failed {name} — {err}");
                    outcome.failed.push((name, err));
                }
            }
        }
    }
    outcome
}

/// Copy one file, directory, or symlink, preferring a reflink so that cloning a
/// multi-gigabyte dependency directory on APFS or Btrfs costs neither time nor
/// disk until something in it is written.
fn copy_path(source: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let (clone, plain): (&[&str], &[&str]) = if cfg!(target_os = "macos") {
        (&["-Rpc"], &["-Rp"])
    } else {
        (&["-a", "--reflink=auto"], &["-a"])
    };
    match cp(clone, source, target) {
        Ok(()) => Ok(()),
        // No reflink support (or no `--reflink` flag at all): copy the bytes.
        Err(reflink_error) => {
            cp(plain, source, target).map_err(
                |err| {
                    if err.is_empty() {
                        reflink_error
                    } else {
                        err
                    }
                },
            )
        }
    }
}

fn cp(flags: &[&str], source: &Path, target: &Path) -> Result<(), String> {
    let output = std::process::Command::new("cp")
        .env("LC_ALL", "C")
        .args(flags)
        .arg(source)
        .arg(target)
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr)
            .lines()
            .next_back()
            .unwrap_or_default()
            .trim()
            .to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// Every checkout git knows about, used to keep a copy from swallowing one.
fn checkouts(repo: &str) -> Vec<PathBuf> {
    git::git_stdout(&["-C", repo, "worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| resolved(Path::new(path)))
        .collect()
}

/// Is `path` a checkout, or a directory holding one? Both sides are resolved
/// first: git reports a checkout by its real path, so a repo reached through a
/// symlink (`/var` → `/private/var`) would otherwise compare as unrelated.
fn holds_checkout(path: &Path, checkouts: &[PathBuf]) -> bool {
    let path = resolved(path);
    checkouts
        .iter()
        .any(|checkout| *checkout == path || checkout.starts_with(&path))
}

fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `herdr-worktrees include`: what a new worktree would receive, and why.
pub fn run_cli(_args: &[String]) -> anyhow::Result<()> {
    let repo = git::repo_root()?;
    let repo = repo.to_string_lossy().into_owned();
    let config = Config::load()?;
    let include = include_file(&repo);

    println!("repo    {repo}");
    if !include.is_file() {
        println!("include {} (missing)", include.display());
        println!("\n  nothing is copied into new worktrees");
        println!("  create the file with .gitignore-style patterns to carry gitignored files over");
        return Ok(());
    }
    println!("include {}", include.display());
    if !config.worktree_include() {
        println!("\n  worktree-include = false; nothing is copied");
        return Ok(());
    }

    let entries = entries(&repo);
    if entries.is_empty() {
        println!("\n  no gitignored files match; nothing is copied");
        println!("  entries must be both gitignored and named by {INCLUDE_FILE}");
        return Ok(());
    }
    println!();
    for entry in &entries {
        let kind = if entry.is_dir() { "dir " } else { "file" };
        println!("  {kind} {}", entry.0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn entry(path: &str) -> Entry {
        Entry(path.to_string())
    }

    #[test]
    fn a_named_entry_is_copied_whole() {
        let ignored = [entry(".env"), entry("node_modules/"), entry("dist/")];
        let matched = [".env".to_string(), "node_modules/".to_string()];
        let selected = select(&ignored, &matched, |leftover| {
            // Only the unnamed directory needs a second look.
            assert_eq!(leftover, ["dist/"]);
            Vec::new()
        });
        assert_eq!(selected, [entry(".env"), entry("node_modules/")]);
    }

    #[test]
    fn a_directory_above_an_entry_covers_it() {
        let ignored = [entry("packages/app/.env"), entry("other/.env")];
        let matched = ["packages/".to_string()];
        let selected = select(&ignored, &matched, |leftover| {
            assert_eq!(leftover, ["other/.env"]);
            Vec::new()
        });
        assert_eq!(selected, [entry("packages/app/.env")]);
    }

    #[test]
    fn a_prefix_match_does_not_leak_across_directories() {
        let ignored = [entry("foobar/x")];
        let selected = select(&ignored, &["foo/".to_string()], |_| Vec::new());
        assert!(selected.is_empty());
    }

    #[test]
    fn patterns_reaching_below_a_collapsed_directory_come_back_from_the_deep_pass() {
        let ignored = [entry("venvish/lib/"), entry("dist/")];
        let selected = select(&ignored, &[], |leftover| {
            assert_eq!(leftover, ["venvish/lib/", "dist/"]);
            vec!["venvish/lib/thing.py".to_string()]
        });
        assert_eq!(selected, [entry("venvish/lib/thing.py")]);
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .args(args)
            .output()
            .expect("git to run");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write(path: PathBuf, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a parent directory")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A repo whose ignored files cover every shape the selection has to handle.
    fn repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-wt-include-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("app");
        std::fs::create_dir_all(&main).unwrap();
        git(&root, &["init", "-q", "-b", "main", "app"]);
        write(main.join("README.md"), "readme");
        write(main.join("tracked.env"), "tracked");
        write(
            main.join(".gitignore"),
            "*.log\n.env\ndist/\nnode_modules/\npackages/app/.env\n",
        );
        write(main.join(INCLUDE_FILE), ".env\nnode_modules/\n");
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "init"]);

        write(main.join(".env"), "secret");
        write(main.join("debug.log"), "noise");
        write(main.join("dist/app.js"), "built");
        write(main.join("node_modules/pkg/index.js"), "dep");
        // An ignored file below an untracked (so uncollapsed) directory.
        write(main.join("packages/app/.env"), "nested secret");
        write(main.join("packages/app/main.rs"), "untracked, not ignored");
        main
    }

    fn config(toml: &str) -> Config {
        toml::from_str(toml).expect("test config to parse")
    }

    fn worktree(main: &Path, name: &str) -> PathBuf {
        let path = main.parent().unwrap().join(name);
        git(
            main,
            &["worktree", "add", "-q", &path.to_string_lossy(), "-b", name],
        );
        path
    }

    #[test]
    fn copies_only_files_that_are_both_ignored_and_named() {
        let main = repo("both");
        let dest = worktree(&main, "feature");
        let outcome = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );

        assert_eq!(
            std::fs::read_to_string(dest.join(".env")).unwrap(),
            "secret"
        );
        assert!(dest.join("node_modules/pkg/index.js").is_file());
        // Ignored, but not named by .worktreeinclude.
        assert!(!dest.join("debug.log").exists());
        assert!(!dest.join("dist").exists());
        // Named by `.env`, ignored, and below an untracked directory.
        assert!(dest.join("packages/app/.env").is_file());
        // Untracked but not ignored: not ours to copy.
        assert!(!dest.join("packages/app/main.rs").exists());
        // Tracked files come from the checkout, never from a copy.
        assert_eq!(outcome.failed, []);
        assert!(!outcome.copied.iter().any(|name| name == "tracked.env"));
    }

    #[test]
    fn an_existing_destination_file_is_left_alone() {
        let main = repo("existing");
        let dest = worktree(&main, "keep");
        write(dest.join(".env"), "worktree-local");

        let outcome = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert_eq!(
            std::fs::read_to_string(dest.join(".env")).unwrap(),
            "worktree-local"
        );
        assert_eq!(outcome.skipped, [".env"]);
    }

    #[test]
    fn a_pattern_below_a_collapsed_directory_still_copies() {
        let main = repo("deep");
        write(main.join(INCLUDE_FILE), "packages/app/.env\n");
        let dest = worktree(&main, "deep-target");
        copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );

        assert!(dest.join("packages/app/.env").is_file());
        assert!(!dest.join(".env").exists());
    }

    #[test]
    fn a_directory_holding_a_checkout_is_never_copied() {
        let main = repo("nested");
        write(main.join(".gitignore"), ".worktrees/\n");
        write(main.join(INCLUDE_FILE), ".worktrees/\n");
        let nested = main.join(".worktrees/inner");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                &nested.to_string_lossy(),
                "-b",
                "inner",
            ],
        );
        let dest = worktree(&main, "nested-target");

        let outcome = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(!dest.join(".worktrees").exists());
        assert_eq!(outcome.skipped, [".worktrees/"]);
    }

    #[test]
    fn nothing_happens_without_the_file_or_with_the_setting_off() {
        let main = repo("off");
        let dest = worktree(&main, "off-target");
        let repo_path = main.to_string_lossy().into_owned();

        assert!(applies(&repo_path, &config("")));
        assert!(!applies(&repo_path, &config("worktree-include = false")));
        let outcome = copy_into(
            &repo_path,
            &dest.to_string_lossy(),
            &config("worktree-include = false"),
        );
        assert!(outcome.is_empty());
        assert!(!dest.join(".env").exists());

        std::fs::remove_file(main.join(INCLUDE_FILE)).unwrap();
        assert!(!applies(&repo_path, &config("")));
        assert!(entries(&repo_path).is_empty());
    }
}
