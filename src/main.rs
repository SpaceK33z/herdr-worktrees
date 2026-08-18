//! Binary entry point.

use herdr_worktrees::tty;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match herdr_worktrees::run(&args) {
        Ok(()) => 0,
        Err(e) => {
            tty::err(&format!("{e:#}"));
            1
        }
    };
    std::process::exit(code);
}
