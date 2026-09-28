//! Process-level facts and the standard streams.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::io::{BufRead, IsTerminal, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A setting from the environment: [name]'s value trimmed, or None when it is unset or blank.
pub fn env_setting(name: &str) -> Option<String> {
    env(name).map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Kernel release and machine, as `uname -r` and `uname -m` say.
pub fn uname() -> (String, String) {
    let names = rustix::system::uname();
    (names.release().to_string_lossy().into_owned(), names.machine().to_string_lossy().into_owned())
}

pub fn out(text: &str) {
    out_bytes(text.as_bytes());
}

pub fn err(text: &str) {
    write_and_flush(std::io::stderr().lock(), text.as_bytes());
}

/// A line on stdout.
pub fn say(text: &str) {
    out(&format!("{text}\n"));
}

/// A line on stderr, the way limen reports what it did or what went wrong: `limen: …`.
pub fn log(message: &str) {
    err(&format!("limen: {message}\n"));
}

pub fn out_bytes(bytes: &[u8]) {
    write_and_flush(std::io::stdout().lock(), bytes);
}

fn write_and_flush(mut stream: impl Write, bytes: &[u8]) {
    stream.write_all(bytes).ok();
    stream.flush().ok();
}

/// One request from stdin, up to [max] bytes and within [timeout]. One request is one line (spec §4): it stops at the
/// newline instead of waiting for the client to close.
pub fn read_stdin(max: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    let mut out = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = Timespec { tv_sec: left.as_secs() as i64, tv_nsec: left.subsec_nanos() as _ };
        // Read from the descriptor itself: std's buffer would hold what poll can't see.
        if poll(&mut [PollFd::new(&fd, PollFlags::IN)], Some(&wait)).unwrap_or(0) == 0 {
            return Err(format!("no request within {}s", timeout.as_secs()));
        }
        let n = rustix::io::read(fd, &mut buffer).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..n]);
        if out.len() > max {
            return Err(format!("request larger than {max} bytes"));
        }
        if buffer[n - 1] == b'\n' {
            break;
        }
    }
    Ok(out)
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

/// What root writes is readable by others and writable by nobody else, whatever umask it was started with.
pub fn umask_022() {
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
}
