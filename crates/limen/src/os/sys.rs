//! Process-level facts and the standard streams.

use std::io::{BufRead, Read, Write};

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Kernel release and machine, as `uname -r` and `uname -m` say.
pub fn uname() -> (String, String) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let field = |f: &[libc::c_char]| {
        let bytes: Vec<u8> = f.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (field(&u.release), field(&u.machine))
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
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as u64 } else { 100 }
}

pub fn is_terminal(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// One line from stdin, without echoing it when stdin is a terminal: for a token typed at a prompt.
pub fn read_secret() -> Option<String> {
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if tty {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        err("\n");
    }
    match read {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
    }
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
