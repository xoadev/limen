//! Process-level facts and the standard streams.

use std::io::{BufRead, IsTerminal, Read, Write};

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Kernel release and machine, as `uname -r` and `uname -m` say.
pub fn uname() -> (String, String) {
    let u = rustix::system::uname();
    (u.release().to_string_lossy().into_owned(), u.machine().to_string_lossy().into_owned())
}

pub fn out(text: &str) {
    let mut o = std::io::stdout().lock();
    o.write_all(text.as_bytes()).ok();
    o.flush().ok();
}

pub fn err(text: &str) {
    let mut e = std::io::stderr().lock();
    e.write_all(text.as_bytes()).ok();
    e.flush().ok();
}

pub fn out_bytes(bytes: &[u8]) {
    let mut o = std::io::stdout().lock();
    o.write_all(bytes).ok();
    o.flush().ok();
}

/// One request from stdin, up to [max] bytes; None when there is more than that. One request is one line (spec §4):
/// it stops at the newline instead of waiting for the client to close.
pub fn read_stdin(max: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = stdin.read(&mut buffer).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..n]);
        if out.len() > max {
            return None;
        }
        if buffer[n - 1] == b'\n' {
            break;
        }
    }
    Some(out)
}

/// Clock ticks per second, the unit of `/proc/<pid>/stat` times.
pub fn ticks_per_second() -> u64 {
    rustix::param::clock_ticks_per_second()
}

pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

/// One line from stdin, without echoing it when stdin is a terminal: for a token typed at a prompt.
pub fn read_secret() -> Option<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin).ok();
    if let Some(mut quiet) = saved.clone() {
        quiet.local_modes.remove(LocalModes::ECHO);
        tcsetattr(&stdin, OptionalActions::Now, &quiet).ok();
    }
    let line = read_line();
    if let Some(saved) = saved {
        tcsetattr(&stdin, OptionalActions::Now, &saved).ok();
        err("\n");
    }
    line
}

/// One line from stdin, as typed.
pub fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
    }
}

pub fn chdir_root() {
    std::env::set_current_dir("/").ok();
}
