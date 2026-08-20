//! Terminal helpers: errors, single-keypress confirmation, and pause-on-close.

use std::io::{IsTerminal, Write};

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

/// Ask for confirmation. `ctrl-x` always confirms; otherwise a guarded prompt
/// requires `ctrl-x` and a normal prompt accepts enter / y / Y. Without a
/// terminal nobody can answer, so the answer is no — these prompts guard
/// worktree removal and running setup scripts from forks.
pub fn confirm(prompt: &str, require_force: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        err("stdin is not a terminal; run interactively to confirm");
        return false;
    }
    println!("\n{prompt}");
    let key = read_key();
    println!();
    if key == Some(0x18) {
        return true;
    }
    if require_force {
        return false;
    }
    // A read error yields `None`, which is a denial like any other non-answer.
    matches!(key, Some(b'\n' | b'\r' | b'y' | b'Y'))
}
