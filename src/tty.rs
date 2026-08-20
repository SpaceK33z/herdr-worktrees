//! Terminal helpers: errors, single-keypress confirmation, pause-on-close,
//! progress spinners, and the small fzf prompts the popup opens over itself.

use indicatif::{ProgressBar, ProgressStyle};
use std::io::{IsTerminal, Write};
use std::time::Duration;

pub fn err(msg: &str) {
    eprintln!("\x1b[31m{msg}\x1b[0m");
}

/// A non-fatal note: the operation continued despite it.
pub fn warn(msg: &str) {
    eprintln!("\x1b[33m{msg}\x1b[0m");
}

/// Read a single byte from the terminal without waiting for Enter and without
/// echo. Returns `None` when stdin is not a TTY (or on any error).
pub fn read_key() -> Option<u8> {
    use std::os::unix::io::AsRawFd;
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    let fd = stdin.as_raw_fd();
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return None;
        }
        let orig = termios;
        termios.c_lflag &= !(libc::ICANON | libc::ECHO);
        termios.c_cc[libc::VMIN] = 1;
        termios.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(fd, libc::TCSANOW, &termios);
        let mut buf = [0u8; 1];
        let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1);
        libc::tcsetattr(fd, libc::TCSANOW, &orig);
        if n == 1 {
            Some(buf[0])
        } else {
            None
        }
    }
}

/// Pause so a popup stays open long enough to read the message.
pub fn wait_key() {
    if std::io::stdin().is_terminal() {
        print!("\npress any key to close");
        let _ = std::io::stdout().flush();
        let _ = read_key();
        println!();
    } else {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
}

/// A ticking spinner for work the user is waiting on. The caller keeps it alive
/// for the duration and calls `finish_and_clear` so the line leaves no trace.
pub fn spinner(message: impl Into<String>) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner:.cyan} {msg}") {
        progress.set_style(style.tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]));
    }
    progress.set_message(message.into());
    progress.enable_steady_tick(Duration::from_millis(80));
    progress
}

/// Open a small fzf prompt over `items` (one candidate per line) and return the
/// chosen line. `None` covers both cancelling and fzf failing to run at all,
/// which callers treat the same way: the action does not happen.
pub fn pick(items: &str, prompt: &str, header: &str, query: &str) -> Option<String> {
    let mut child = std::process::Command::new("fzf")
        .args([
            "--ansi",
            "--reverse",
            "--info=inline",
            "--border=rounded",
            &format!("--prompt={prompt} ❯ "),
            &format!("--header={header}"),
            "--query",
            query,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(items.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!choice.is_empty()).then_some(choice)
}

/// Ask for confirmation: enter / y / Y confirms, any other key cancels.
/// Without a terminal nobody can answer, so the answer is no — these prompts
/// guard worktree removal and running setup scripts from forks.
pub fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        err("stdin is not a terminal; run interactively to confirm");
        return false;
    }
    println!("\n{prompt}");
    let key = read_key();
    println!();
    // A read error yields `None`, which is a denial like any other non-answer.
    matches!(key, Some(b'\n' | b'\r' | b'y' | b'Y'))
}
