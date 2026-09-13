//! herdr-worktrees — switch, create, and remove git worktrees from a Herdr popup.
//!
//! Library entry point; the binary lives in `main.rs`.

pub mod background;
pub mod config;
pub mod create;
pub mod detect;
pub mod git;
pub mod herdr;
pub mod include;
pub mod model;
pub mod open;
pub mod picker;
pub mod pr;
pub mod remove;
pub mod render;
pub mod row;
pub mod setup;
pub mod status;
pub mod theme;
pub mod tty;
pub mod update;
pub mod util;

use anyhow::Result;

const USAGE: &str = concat!(
    "usage: herdr-worktrees [open|picker|create|remover|remove|setup|detect|include] [args...]\n",
    "       herdr-worktrees create <branch> [--base <ref>] [--exact] [--json] [--no-setup]\n",
    "       herdr-worktrees remove --target <branch> <path> [--yes|-y] [--force|-f]\n",
    "       herdr-worktrees [--json|--fzf|--header] [--no-cache|--no-detached|--fast]"
);

/// The flags `model::run_engine` understands; anything else in first position
/// is a typo rather than an engine invocation.
const ENGINE_FLAGS: [&str; 6] = [
    "--json",
    "--fzf",
    "--header",
    "--no-cache",
    "--no-detached",
    "--fast",
];

/// Dispatch the command-line entry points (see `main.rs`).
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("open") => open::run(&args[1..]),
        Some("create") => create::run_cli(&args[1..]),
        Some("picker") => picker::run(&args[1..]),
        Some("picker-cache-list") => picker::run_cached_list(&args[1..]),
        Some("picker-cache-refresh") => picker::run_cache_refresh(&args[1..]),
        Some("picker-cache-fetch") => picker::run_cache_fetch(&args[1..]),
        Some("picker-footer") => picker::run_footer(&args[1..]),
        Some("remover") => remove::run_interactive(),
        Some("remove-list") => remove::run_list(&args[1..]),
        Some("remove") => remove::run_target(&args[1..]),
        Some("remove-bg-batch") => remove::run_background_batch(&args[1..]),
        Some("remove-progress") => remove::run_progress(&args[1..]),
        Some("detect") => detect::run_cli(&args[1..]),
        Some("include") => include::run_cli(&args[1..]),
        Some("setup") => setup::run_cli(&args[1..]),
        Some("setup-bg") => setup::run_background(&args[1..]),
        Some("--version" | "-V") => {
            println!("herdr-worktrees {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(unknown) if !ENGINE_FLAGS.contains(&unknown) => {
            anyhow::bail!("unknown argument '{unknown}'\n{USAGE}")
        }
        _ => model::run_engine(args),
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn reports_unknown_arguments_instead_of_running_the_engine() {
        let error = run(&args(&["--verison"])).unwrap_err().to_string();
        assert!(error.contains("unknown argument '--verison'"), "{error}");
        assert!(error.contains("usage:"), "{error}");

        let error = run(&args(&["picke"])).unwrap_err().to_string();
        assert!(error.contains("unknown argument 'picke'"), "{error}");
    }

    #[test]
    fn prints_version_and_help() {
        assert!(run(&args(&["--version"])).is_ok());
        assert!(run(&args(&["-V"])).is_ok());
        assert!(run(&args(&["--help"])).is_ok());
        assert!(run(&args(&["-h"])).is_ok());
    }
}
