//! herdr-worktrees — switch, create, and remove git worktrees from a Herdr popup.
//!
//! Library entry point; the binary lives in `main.rs`.

pub mod background;
pub mod config;
pub mod git;
pub mod herdr;
pub mod model;
pub mod open;
pub mod picker;
pub mod pr;
pub mod remove;
pub mod render;
pub mod setup;
pub mod status;
pub mod theme;
pub mod tty;
pub mod util;

use anyhow::Result;

/// Dispatch the command-line entry points (see `main.rs`).
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("open") => open::run(&args[1..]),
        Some("picker") => picker::run(&args[1..]),
        Some("remover") => remove::run_interactive(),
        Some("remove-list") => remove::run_list(&args[1..]),
        Some("remove") => remove::run_target(&args[1..]),
        Some("remove-bg-batch") => remove::run_background_batch(&args[1..]),
        Some("remove-progress") => remove::run_progress(&args[1..]),
        Some("setup") => setup::run_cli(&args[1..]),
        Some("setup-bg") => setup::run_background(&args[1..]),
        _ => model::run_engine(args),
    }
}
