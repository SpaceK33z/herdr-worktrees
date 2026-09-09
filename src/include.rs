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
#[derive(Debug, Default, Clone, serde::Serialize)]
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
        let target = destination_parent(Path::new(dest), Path::new(entry.relative()));
        // A `.worktreeinclude` broad enough to name an ignored `.worktrees/`
        // would otherwise copy other checkouts into this one.
        if holds_checkout(&source, &checkouts) {
            eprintln!("include: skipped {name} (holds a worktree)");
            outcome.skipped.push(name);
        } else {
            match target.and_then(|(parent, leaf)| copy_path(&source, &parent, &leaf)) {
                Ok(false) => {
                    eprintln!("include: skipped {name} (already present)");
                    outcome.skipped.push(name);
                }
                Ok(true) => {
                    eprintln!("include: copied {name}");
                    outcome.copied.push(name);
                }
                Err(err) => {
                    eprintln!("include: failed {name} — {err}");
                    outcome.failed.push((name, err));
                }
            }
        }
    }
    outcome
}

// Destination access is descriptor-relative throughout. Each ancestor is opened
// with O_NOFOLLOW; swapping a checked directory for a symlink cannot redirect
// a subsequent write. Renaming an already-open directory elsewhere is outside
// the symlink threat model (requires concurrent control of the checkout).
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

fn c_name(name: &OsStr) -> Result<CString, String> {
    CString::new(name.as_bytes()).map_err(|e| e.to_string())
}

fn open_directory(parent: &File, name: &OsStr) -> Result<File, String> {
    let name = c_name(name)?;
    // SAFETY: name is NUL terminated; parent is live. We take ownership only
    // of a successful new descriptor, never of the parent's descriptor.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn mkdir(parent: &File, name: &OsStr) -> Result<bool, String> {
    let name = c_name(name)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        Ok(false)
    } else {
        Err(error.to_string())
    }
}

fn destination_parent(root: &Path, relative: &Path) -> Result<(File, std::ffi::OsString), String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => parts.push(name),
            _ => return Err("include path must stay below the checkout root".into()),
        }
    }
    let leaf = parts.pop().ok_or("empty include path")?.to_owned();
    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|e| e.to_string())?;
    for name in parts {
        let created = mkdir(&parent, name)?;
        parent = open_directory(&parent, name)?;
        if created {
            // A destination default ACL may suppress the owner's execute bit.
            make_private(&parent, true)?;
        }
    }
    Ok((parent, leaf))
}

/// Copy without following or replacing any existing destination. All recursive
/// children are newly created and opened no-follow too; cp's path traversal is
/// deliberately not used for destination access. Hardlinked source files may
/// become independent copies; this is not a full archival `cp -a` replacement.
fn copy_path(source: &Path, parent: &File, leaf: &OsStr) -> Result<bool, String> {
    let metadata = source.symlink_metadata().map_err(|e| e.to_string())?;
    let name = c_name(leaf)?;
    if metadata.is_dir() {
        if !mkdir(parent, leaf)? {
            return Ok(false);
        }
        let dir = open_directory(parent, leaf)?;
        make_private(&dir, true)?;
        let input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(source)
            .map_err(|e| e.to_string())?;
        for child in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let child = child.map_err(|e| e.to_string())?;
            copy_path(&child.path(), &dir, &child.file_name())?;
        }
        copy_metadata(&input, &dir, &metadata)?;
    } else if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(source).map_err(|e| e.to_string())?;
        let target = c_name(target.as_os_str())?;
        if unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } != 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(e.to_string())
            };
        }
        copy_symlink_times(parent, &name, &metadata)?;
    } else if metadata.is_file() {
        let mut input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)
            .map_err(|e| e.to_string())?;
        // APFS cloning can expose source modes under an inherited group. Keep
        // the clone private until group/ACL metadata is established, then
        // publish exclusively. fcopyfile cannot reflink an open destination.
        #[cfg(target_os = "macos")]
        if let Some(copied) = copy_macos_clone(&input, parent, &name, &metadata)? {
            return Ok(copied);
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(e.to_string())
            };
        }
        let mut output = unsafe { File::from_raw_fd(fd) };
        make_private(&output, false)?;
        #[cfg(target_os = "linux")]
        let cloned =
            unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } == 0;
        #[cfg(target_os = "macos")]
        let cloned = unsafe {
            libc::fcopyfile(
                input.as_raw_fd(),
                output.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            )
        } == 0;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let cloned = false;
        if !cloned {
            // A failed clone/copyfile may have partially written or advanced
            // descriptors; the byte-copy fallback starts from a clean file.
            use std::io::{Seek, SeekFrom};
            input.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            output.set_len(0).map_err(|e| e.to_string())?;
            output.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        }
        copy_metadata(&input, &output, &metadata)?;
    } else {
        return Err("unsupported include file type".into());
    }
    Ok(true)
}

fn copy_symlink_times(
    parent: &File,
    name: &CString,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    let times = [
        libc::timespec {
            tv_sec: metadata.atime() as libc::time_t,
            tv_nsec: metadata.atime_nsec() as libc::c_long,
        },
        libc::timespec {
            tv_sec: metadata.mtime() as libc::time_t,
            tv_nsec: metadata.mtime_nsec() as libc::c_long,
        },
    ];
    // Never follow the newly created link (including a dangling link), or
    // resolve its destination through a pathname ancestor again.
    if unsafe {
        libc::utimensat(
            parent.as_raw_fd(),
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn preserve_group(output: &File, gid: libc::gid_t) -> Result<(), String> {
    if output.metadata().map_err(|e| e.to_string())?.gid() != gid
        && unsafe { libc::fchown(output.as_raw_fd(), !0 as libc::uid_t, gid) } != 0
    {
        return Err(format!(
            "preserving source group {gid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if output.metadata().map_err(|e| e.to_string())?.gid() != gid {
        return Err(format!("source group {gid} was not preserved"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos_acl {
    // libc does not currently expose Darwin's descriptor-based ACL API.
    unsafe extern "C" {
        pub fn acl_init(count: libc::c_int) -> *mut libc::c_void;
        pub fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
        pub fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
}

fn make_private(file: &File, directory: bool) -> Result<(), String> {
    file.set_permissions(std::fs::Permissions::from_mode(if directory {
        0o700
    } else {
        0o600
    }))
    .map_err(|e| e.to_string())?;
    // Darwin ACL allow entries are not masked by chmod, unlike POSIX ACLs.
    // Clear inherited ACLs before placing data inside a private entry.
    #[cfg(target_os = "macos")]
    unsafe {
        let acl = macos_acl::acl_init(0);
        if acl.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let result = macos_acl::acl_set_fd(file.as_raw_fd(), acl);
        let error = std::io::Error::last_os_error();
        macos_acl::acl_free(acl);
        if result != 0 && error.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(error.to_string());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_macos_clone(
    input: &File,
    parent: &File,
    name: &CString,
    metadata: &std::fs::Metadata,
) -> Result<Option<bool>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staging_name = (0..100)
        .find_map(|_| {
            let name = format!(
                ".herdr-include-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            match mkdir(parent, OsStr::new(&name)) {
                Ok(true) => Some(Ok(name)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .ok_or("could not reserve a private clone directory")??;
    let staging_cname = c_name(OsStr::new(&staging_name))?;
    let result = (|| {
        let staging = open_directory(parent, OsStr::new(&staging_name))?;
        make_private(&staging, true)?;
        let clone_name = c_name(OsStr::new("copy"))?;
        let result = (|| {
            if unsafe {
                libc::fclonefileat(
                    input.as_raw_fd(),
                    staging.as_raw_fd(),
                    clone_name.as_ptr(),
                    0,
                )
            } != 0
            {
                return Ok(None); // Unsupported cloning falls back to byte copying.
            }
            let fd = unsafe {
                libc::openat(
                    staging.as_raw_fd(),
                    clone_name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let output = unsafe { File::from_raw_fd(fd) };
            copy_metadata(input, &output, metadata)?;
            if unsafe {
                libc::renameatx_np(
                    staging.as_raw_fd(),
                    clone_name.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_EXCL,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                return if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(Some(false))
                } else {
                    Err(error.to_string())
                };
            }
            Ok(Some(true))
        })();
        // Always unlink relative to the pinned private directory, never through
        // its visible pathname; this also cleans partial/failed clones.
        if unsafe { libc::unlinkat(staging.as_raw_fd(), clone_name.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.to_string());
            }
        }
        result
    })();
    if unsafe {
        libc::unlinkat(
            parent.as_raw_fd(),
            staging_cname.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    result
}

/// Preserve the owning group before ACLs and modes: an ACL's group:: entry
/// names the inode's group, whereas mode group bits may only be an ACL mask.
/// Copying either without its permission identity can expose secrets. Errors
/// leave the new entry private and are reported as copy failures.
fn copy_metadata(input: &File, output: &File, metadata: &std::fs::Metadata) -> Result<(), String> {
    let result = (|| {
        preserve_group(output, metadata.gid())?;
        #[cfg(target_os = "linux")]
        copy_linux_xattrs(input, output)?;
        #[cfg(target_os = "macos")]
        if unsafe {
            libc::fcopyfile(
                input.as_raw_fd(),
                output.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        output
            .set_permissions(metadata.permissions())
            .map_err(|e| e.to_string())?;
        let times = std::fs::FileTimes::new()
            .set_accessed(metadata.accessed().map_err(|e| e.to_string())?)
            .set_modified(metadata.modified().map_err(|e| e.to_string())?);
        output.set_times(times).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        make_private(output, metadata.is_dir())?;
    }
    result
}

#[cfg(target_os = "linux")]
fn linux_xattrs(input: &File) -> Result<Vec<(CString, Vec<u8>)>, String> {
    // Kernel-sized buffers; an attribute changing between size/read calls
    // fails closed instead of silently omitting metadata.
    let size = unsafe { libc::flistxattr(input.as_raw_fd(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOTSUP) {
            Ok(Vec::new())
        } else {
            Err(e.to_string())
        };
    }
    let mut names = vec![0u8; size as usize];
    let size =
        unsafe { libc::flistxattr(input.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) };
    if size < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if size as usize > names.len() {
        return Err("attribute list changed during copy".into());
    }
    names.truncate(size as usize);
    let mut attributes = Vec::new();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        let size =
            unsafe { libc::fgetxattr(input.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut value = vec![0u8; size as usize];
        let size = unsafe {
            libc::fgetxattr(
                input.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if size < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if size as usize > value.len() {
            return Err("attribute changed during copy".into());
        }
        value.truncate(size as usize);
        attributes.push((name, value));
    }
    Ok(attributes)
}

#[cfg(target_os = "linux")]
fn copy_linux_xattrs(input: &File, output: &File) -> Result<(), String> {
    let attributes = linux_xattrs(input)?;
    // New entries may have inherited ACLs from the destination. They must not
    // gain those ACLs' permissions when we subsequently apply source mode bits.
    for acl in ["system.posix_acl_access", "system.posix_acl_default"] {
        let name = CString::new(acl).expect("static ACL name");
        if !attributes.iter().any(|(source, _)| source == &name)
            && unsafe { libc::fremovexattr(output.as_raw_fd(), name.as_ptr()) } != 0
        {
            let e = std::io::Error::last_os_error();
            if !matches!(e.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP)) {
                return Err(e.to_string());
            }
        }
    }
    for (name, value) in attributes {
        if unsafe {
            libc::fsetxattr(
                output.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(format!(
                "preserving {}: {}",
                name.to_string_lossy(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
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
    #[test]
    fn symlinked_destination_ancestors_cannot_receive_secrets() {
        let main = repo("symlink-parent");
        let dest = worktree(&main, "symlink-target");
        let outside = main.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("packages")).unwrap();
        let outcome = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(outcome
            .failed
            .iter()
            .any(|(name, _)| name == "packages/app/.env"));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn replacing_an_open_ancestor_with_a_symlink_does_not_redirect_copy() {
        let main = repo("symlink-race");
        let dest = worktree(&main, "race-target");
        let outside = main.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let (parent, leaf) = destination_parent(&dest, Path::new("config/secret")).unwrap();
        std::fs::rename(dest.join("config"), dest.join("original-config")).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("config")).unwrap();
        assert!(copy_path(&main.join(".env"), &parent, &leaf).unwrap());
        assert!(!outside.join("secret").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("original-config/secret")).unwrap(),
            "secret"
        );
        // An attacker winning the final-component race is not followed either.
        std::os::unix::fs::symlink(outside.join("stolen"), dest.join("original-config/another"))
            .unwrap();
        assert!(!copy_path(&main.join(".env"), &parent, OsStr::new("another")).unwrap());
        assert!(!outside.join("stolen").exists());
        assert!(destination_parent(&dest, Path::new("../escape")).is_err());
    }
    #[test]
    fn copies_executable_modes_file_and_directory_times_and_symlinks() {
        use std::time::{Duration, SystemTime};
        let main = repo("metadata");
        let source = main.join("node_modules");
        let file = source.join("pkg/index.js");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("pkg/index.js", source.join("link")).unwrap();
        let timestamp = SystemTime::UNIX_EPOCH + Duration::new(1_000_000, 123_456_789);
        let times = std::fs::FileTimes::new()
            .set_modified(timestamp)
            .set_accessed(timestamp);
        File::open(&file).unwrap().set_times(times).unwrap();
        File::open(&source).unwrap().set_times(times).unwrap();
        copy_symlink_times(
            &File::open(&source).unwrap(),
            &c_name(OsStr::new("link")).unwrap(),
            &file.metadata().unwrap(),
        )
        .unwrap();
        let dest = worktree(&main, "metadata-target");
        let result = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(result.failed.is_empty(), "{:?}", result.failed);
        let target = dest.join("node_modules/pkg/index.js");
        assert_eq!(
            target.metadata().unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(target.metadata().unwrap().modified().unwrap(), timestamp);
        assert_eq!(
            dest.join("node_modules")
                .metadata()
                .unwrap()
                .modified()
                .unwrap(),
            timestamp
        );
        let link_metadata = dest.join("node_modules/link").symlink_metadata().unwrap();
        assert_eq!(link_metadata.modified().unwrap(), timestamp);
        assert_eq!(link_metadata.accessed().unwrap(), timestamp);
        assert_eq!(
            std::fs::read_link(dest.join("node_modules/link")).unwrap(),
            Path::new("pkg/index.js")
        );
    }

    #[test]
    fn symlink_times_do_not_touch_external_targets_and_support_dangling_links() {
        let main = repo("symlink-times");
        let outside = main.parent().unwrap().join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        let outside_times = outside.metadata().unwrap();
        let reference = File::open(main.join(".env")).unwrap();
        reference
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
            ))
            .unwrap();
        let expected = reference.metadata().unwrap();
        let source_parent = File::open(&main).unwrap();
        let dest = worktree(&main, "symlink-times-target");
        let dest_parent = File::open(&dest).unwrap();
        for (name, target) in [
            ("external", outside.clone()),
            ("dangling", main.join("missing")),
        ] {
            std::os::unix::fs::symlink(&target, main.join(name)).unwrap();
            copy_symlink_times(
                &source_parent,
                &c_name(OsStr::new(name)).unwrap(),
                &expected,
            )
            .unwrap();
            assert!(copy_path(&main.join(name), &dest_parent, OsStr::new(name)).unwrap());
            assert_eq!(
                dest.join(name)
                    .symlink_metadata()
                    .unwrap()
                    .modified()
                    .unwrap(),
                expected.modified().unwrap()
            );
            assert_eq!(std::fs::read_link(dest.join(name)).unwrap(), target);
        }
        assert_eq!(
            outside.metadata().unwrap().modified().unwrap(),
            outside_times.modified().unwrap()
        );
        assert_eq!(
            outside.metadata().unwrap().accessed().unwrap(),
            outside_times.accessed().unwrap()
        );
    }

    fn alternate_test_group(source_gid: libc::gid_t) -> Option<libc::gid_t> {
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        assert_eq!(
            unsafe { libc::getgroups(count, groups.as_mut_ptr()) },
            count
        );
        let alternate = groups.into_iter().find(|gid| *gid != source_gid);
        if alternate.is_none() {
            eprintln!("setgid regression needs membership in a second group");
        }
        alternate
    }

    #[test]
    fn files_and_directories_preserve_owning_group_under_setgid_destination() {
        let main = repo("owning-group");
        let source_gid = main.metadata().unwrap().gid();
        let Some(destination_gid) = alternate_test_group(source_gid) else {
            return;
        };
        let source_file = File::open(main.join(".env")).unwrap();
        let source_dir = File::open(main.join("node_modules")).unwrap();
        source_file
            .set_permissions(std::fs::Permissions::from_mode(0o640))
            .unwrap();
        source_dir
            .set_permissions(std::fs::Permissions::from_mode(0o750))
            .unwrap();
        #[cfg(target_os = "linux")]
        {
            let mut acl = test_acl();
            acl[14] = 4; // owning group has read access, not merely an ACL mask
            if !set_test_xattr(&source_file, "system.posix_acl_access", &acl) {
                return;
            }
            acl[6] = 7;
            acl[14] = 5;
            acl[30] = 5;
            assert!(set_test_xattr(&source_dir, "system.posix_acl_access", &acl));
        }
        #[cfg(target_os = "macos")]
        for path in [main.join(".env"), main.join("node_modules")] {
            assert!(Command::new("chmod")
                .args(["+a", "everyone allow readattr"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        let dest = worktree(&main, "owning-group-target");
        let parent = File::open(&dest).unwrap();
        preserve_group(&parent, destination_gid).unwrap();
        parent
            .set_permissions(std::fs::Permissions::from_mode(0o2700))
            .unwrap();
        let result = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(result.failed.is_empty(), "{:?}", result.failed);
        for name in [
            ".env",
            "node_modules",
            "node_modules/pkg",
            "node_modules/pkg/index.js",
        ] {
            let input = File::open(main.join(name)).unwrap();
            let output = File::open(dest.join(name)).unwrap();
            assert_eq!(
                output.metadata().unwrap().gid(),
                input.metadata().unwrap().gid(),
                "{name}"
            );
            assert_eq!(
                output.metadata().unwrap().mode() & 0o7777,
                input.metadata().unwrap().mode() & 0o7777,
                "{name}"
            );
            #[cfg(target_os = "linux")]
            for attr in linux_xattrs(&input).unwrap() {
                assert!(linux_xattrs(&output).unwrap().contains(&attr), "{name}");
            }
            #[cfg(target_os = "macos")]
            assert_eq!(
                macos_test_acl(&main.join(name)),
                macos_test_acl(&dest.join(name))
            );
        }
        assert!(!std::fs::read_dir(&dest).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".herdr-include-")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_group_preservation_failure_does_not_install_source_acl_or_mode() {
        let main = repo("group-failure");
        let input = File::open(main.join(".env")).unwrap();
        let Some(other_gid) = alternate_test_group(input.metadata().unwrap().gid()) else {
            return;
        };
        let mut acl = test_acl();
        acl[14] = 4;
        if !set_test_xattr(&input, "system.posix_acl_access", &acl) {
            return;
        }
        let target = main.join("private-copy");
        let output = File::create(&target).unwrap();
        make_private(&output, false).unwrap();
        preserve_group(&output, other_gid).unwrap();
        // O_PATH permits inspection but rejects fchown: simulate inability to
        // preserve the required group without process-wide privilege changes.
        let denied = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&target)
            .unwrap();
        assert!(copy_metadata(&input, &denied, &input.metadata().unwrap()).is_err());
        assert_eq!(output.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(output.metadata().unwrap().gid(), other_gid);
        assert!(linux_xattrs(&output).unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_entries_remove_inherited_allow_acls() {
        let main = repo("private-acl");
        let parent = File::open(&main).unwrap();
        assert!(Command::new("chmod")
            .args([
                "+a",
                "everyone allow read,readattr,search,file_inherit,directory_inherit"
            ])
            .arg(&main)
            .status()
            .unwrap()
            .success());
        assert!(mkdir(&parent, OsStr::new("private")).unwrap());
        let private = open_directory(&parent, OsStr::new("private")).unwrap();
        assert!(!macos_test_acl(&main.join("private")).is_empty());
        make_private(&private, true).unwrap();
        assert!(macos_test_acl(&main.join("private")).is_empty());
        assert_eq!(private.metadata().unwrap().mode() & 0o777, 0o700);
    }

    #[cfg(target_os = "macos")]
    fn macos_test_acl(path: &Path) -> String {
        let output = Command::new("ls").arg("-lde").arg(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_clone_publication_is_exclusive_and_cleans_private_staging() {
        let main = repo("clone-exclusive");
        let parent = File::open(&main).unwrap();
        let input = File::open(main.join(".env")).unwrap();
        let name = c_name(OsStr::new("published")).unwrap();
        let result = copy_macos_clone(&input, &parent, &name, &input.metadata().unwrap()).unwrap();
        if result.is_none() {
            eprintln!("filesystem does not support APFS cloning");
            return;
        }
        assert_eq!(result, Some(true));
        std::fs::write(main.join("published"), "existing").unwrap();
        assert_eq!(
            copy_macos_clone(&input, &parent, &name, &input.metadata().unwrap()).unwrap(),
            Some(false)
        );
        assert_eq!(
            std::fs::read_to_string(main.join("published")).unwrap(),
            "existing"
        );
        std::fs::remove_file(main.join("published")).unwrap();
        std::os::unix::fs::symlink(".env", main.join("published")).unwrap();
        assert_eq!(
            copy_macos_clone(&input, &parent, &name, &input.metadata().unwrap()).unwrap(),
            Some(false)
        );
        assert_eq!(
            std::fs::read_link(main.join("published")).unwrap(),
            Path::new(".env")
        );
        assert_eq!(
            std::fs::read_to_string(main.join(".env")).unwrap(),
            "secret"
        );
        assert!(!std::fs::read_dir(&main).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".herdr-include-")));
    }

    #[cfg(target_os = "linux")]
    fn set_test_xattr(file: &File, name: &str, value: &[u8]) -> bool {
        let name = CString::new(name).unwrap();
        if unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } == 0
        {
            return true;
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(error.raw_os_error(), Some(libc::ENOTSUP), "{error}");
        eprintln!("filesystem does not support test xattrs/ACLs");
        false
    }

    #[cfg(target_os = "linux")]
    fn test_acl() -> Vec<u8> {
        // Owning group has NO access, but the ACL mask (and stat's group mode
        // bits) is read. Dropping this ACL and copying mode 0640 widens access.
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (1u16, 6u16, u32::MAX),
            (4, 0, u32::MAX),
            (8, 4, 12345),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&perm.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        acl
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_acl_mask_and_xattrs_survive_without_granting_owning_group_access() {
        let main = repo("acl");
        let input = File::open(main.join(".env")).unwrap();
        if !set_test_xattr(&input, "system.posix_acl_access", &test_acl()) {
            return;
        }
        assert!(set_test_xattr(
            &input,
            "user.herdr-test",
            b"retained metadata"
        ));
        assert_eq!(
            input.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
        let source_dir = File::open(main.join("node_modules")).unwrap();
        let mut directory_acl = test_acl();
        directory_acl[6] = 7; // owner needs search permission for recursion
        assert!(set_test_xattr(
            &source_dir,
            "system.posix_acl_access",
            &directory_acl
        ));
        assert!(set_test_xattr(
            &source_dir,
            "system.posix_acl_default",
            &directory_acl
        ));
        assert!(set_test_xattr(
            &source_dir,
            "user.herdr-directory",
            b"directory metadata"
        ));
        let dest = worktree(&main, "acl-target");
        let result = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(result.failed.is_empty(), "{:?}", result.failed);
        let output = File::open(dest.join(".env")).unwrap();
        let directory_attrs =
            linux_xattrs(&File::open(dest.join("node_modules")).unwrap()).unwrap();
        for attribute in linux_xattrs(&source_dir).unwrap() {
            assert!(directory_attrs.contains(&attribute));
        }
        let attrs = linux_xattrs(&output).unwrap();
        for (name, bytes) in linux_xattrs(&input).unwrap() {
            assert!(attrs.contains(&(name, bytes)));
        }
        assert_eq!(
            output.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_inherited_destination_acls_are_removed_when_absent_on_source() {
        let main = repo("inherited-acl");
        let dest = worktree(&main, "inherited-target");
        let parent = File::open(&dest).unwrap();
        if !set_test_xattr(&parent, "system.posix_acl_default", &test_acl()) {
            return;
        }
        let result = copy_into(
            &main.to_string_lossy(),
            &dest.to_string_lossy(),
            &config(""),
        );
        assert!(result.failed.is_empty(), "{:?}", result.failed);
        for target in [dest.join(".env"), dest.join("node_modules")] {
            let attrs = linux_xattrs(&File::open(target).unwrap()).unwrap();
            assert!(!attrs
                .iter()
                .any(|(name, _)| name.to_bytes().starts_with(b"system.posix_acl_")));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_metadata_write_failure_is_reported_and_does_not_apply_broad_source_mode() {
        let main = repo("metadata-failure");
        let input = File::open(main.join(".env")).unwrap();
        if !set_test_xattr(&input, "user.herdr-test", b"required metadata") {
            return;
        }
        let path = main.join("failed-copy");
        File::create(&path)
            .unwrap()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        // O_PATH fault-injects an unusable metadata-write FD, without changing
        // global process state or touching any non-fixture files.
        let output = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&path)
            .unwrap();
        assert!(copy_metadata(&input, &output, &input.metadata().unwrap()).is_err());
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
