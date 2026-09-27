//! Child processes without a shell (spec §12): an argument array, stdout and stderr on separate pipes read with a
//! cap, a timeout that stops the whole process group, and an environment that is exactly the one given.

use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(2);

/// What a command the gate runs sees: a fixed PATH, C locale with UTF-8, UTC, no pagers or colours.
pub const SYSTEM_ENV: &[&str] = &[
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "LANG=C.UTF-8",
    "LC_ALL=C.UTF-8",
    "TZ=UTC",
    "SYSTEMD_PAGER=cat",
    "SYSTEMD_COLORS=0",
    "PAGER=cat",
    "NO_COLOR=1",
];

/// What root's programs and scripts get: [SYSTEM_ENV] and root's home.
pub fn root_env() -> Vec<String> {
    SYSTEM_ENV.iter().map(|s| s.to_string()).chain(["HOME=/root".to_string()]).collect()
}

pub fn system_env() -> Vec<String> {
    SYSTEM_ENV.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone)]
pub struct ProcResult {
    /// Exit status, or -1 when a signal ended the process or its end couldn't be learnt.
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// Output went over the cap: the process was stopped and what came after is lost.
    pub truncated: bool,
}

impl ProcResult {
    pub fn ok(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Sees every block of output as it arrives, fd 1 or 2.
pub type OnChunk<'a> = &'a mut dyn FnMut(i32, &[u8]);

/// How to run: the defaults are a clean system environment, no input, a minute and 16 MiB of output.
pub struct Run<'a> {
    pub env: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub max_output: usize,
    /// Sees every block of output as it arrives, fd 1 or 2: how `apply` streams. When given, output is not kept.
    pub on_chunk: Option<OnChunk<'a>>,
}

impl Default for Run<'_> {
    fn default() -> Self {
        Self { env: system_env(), stdin: None, timeout: Duration::from_secs(60), max_output: 16 << 20, on_chunk: None }
    }
}

/// Runs [argv] to completion. `argv[0]` is an absolute path (see [which]).
pub fn run(argv: &[String], mut opts: Run) -> Result<ProcResult, String> {
    let program = argv
        .first()
        .filter(|p| p.starts_with('/'))
        .ok_or_else(|| format!("argv[0] must be an absolute path: {argv:?}"))?;
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(opts.env.iter().filter_map(|kv| kv.split_once('=')))
        .stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout reaches what a script started too.
        .process_group(0);
    let mut child = command.spawn().map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut running = Running {
        pid: child.id() as i32,
        out: Vec::new(),
        err: Vec::new(),
        truncated: false,
        timed_out: false,
        max_output: opts.max_output,
    };
    let status = running.await_child(&mut child, opts.stdin.take(), opts.timeout, &mut opts.on_chunk);
    let (exit_code, signal) = match status {
        Some(s) => match s.code() {
            Some(code) => (code, None),
            None => (-1, s.signal()),
        },
        None => (-1, None),
    };
    Ok(ProcResult {
        exit_code,
        signal,
        stdout: running.out,
        stderr: running.err,
        timed_out: running.timed_out,
        truncated: running.truncated,
    })
}

struct Running {
    pid: i32,
    out: Vec<u8>,
    err: Vec<u8>,
    truncated: bool,
    timed_out: bool,
    max_output: usize,
}

impl Running {
    fn await_child(
        &mut self,
        child: &mut Child,
        input: Option<Vec<u8>>,
        timeout: Duration,
        on_chunk: &mut Option<OnChunk>,
    ) -> Option<ExitStatus> {
        let start = Instant::now();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stdin = child.stdin.take();
        if let Some(fd) = stdin.as_ref().map(AsRawFd::as_raw_fd) {
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        }
        let input = input.unwrap_or_default();
        let mut offset = 0;
        let mut status = None;
        // After the child exits, what a grandchild keeps open must not hold the answer: drain what is there and stop.
        let mut exited_at: Option<Instant> = None;
        let mut buffer = vec![0u8; CHUNK];
        while stdout.is_some() || stderr.is_some() {
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
            if status.is_some() && exited_at.is_none() {
                exited_at = Some(Instant::now());
            }
            if exited_at.is_some_and(|t| t.elapsed() > Duration::from_millis(200)) {
                break;
            }
            if start.elapsed() > timeout {
                self.timed_out = true;
                self.stop();
                break;
            }
            let mut fds = Vec::with_capacity(3);
            for (pipe, fd) in
                [(1, stdout.as_ref().map(AsRawFd::as_raw_fd)), (2, stderr.as_ref().map(AsRawFd::as_raw_fd))]
            {
                if let Some(fd) = fd {
                    fds.push((pipe, libc::pollfd { fd, events: libc::POLLIN, revents: 0 }));
                }
            }
            if let Some(fd) = stdin.as_ref().map(AsRawFd::as_raw_fd) {
                fds.push((0, libc::pollfd { fd, events: libc::POLLOUT, revents: 0 }));
            }
            let mut raw: Vec<libc::pollfd> = fds.iter().map(|(_, p)| *p).collect();
            let ready = unsafe { libc::poll(raw.as_mut_ptr(), raw.len() as libc::nfds_t, 50) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                self.stop();
                break;
            }
            for (i, (pipe, _)) in fds.iter().enumerate() {
                let revents = raw[i].revents;
                if revents == 0 {
                    continue;
                }
                if *pipe == 0 {
                    // The child may close its stdin early: EPIPE is an answer, not a crash (SIGPIPE is ignored).
                    let done = match stdin.as_mut().map(|s| s.write(&input[offset..])) {
                        Some(Ok(n)) => {
                            offset += n;
                            offset >= input.len()
                        }
                        Some(Err(e)) => e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::Interrupted,
                        None => true,
                    };
                    if done || offset >= input.len() {
                        stdin = None;
                    }
                    continue;
                }
                let fd = raw[i].fd;
                let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), CHUNK) };
                if n > 0 {
                    self.accept(*pipe, &buffer[..n as usize], on_chunk);
                } else if n == 0 || std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
                    if *pipe == 1 { stdout = None } else { stderr = None }
                }
            }
            if input.is_empty() {
                stdin = None;
            }
            if self.truncated {
                self.stop();
                break;
            }
        }
        drop((stdout, stderr, stdin));
        if status.is_none() {
            // The pipes closed or the child was told to stop: give it the grace period, then kill the group.
            let deadline = Instant::now() + KILL_GRACE;
            while status.is_none() && Instant::now() < deadline {
                status = child.try_wait().ok().flatten();
                if status.is_none() {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            if status.is_none() {
                unsafe { libc::kill(-self.pid, libc::SIGKILL) };
                status = child.wait().ok();
            }
        }
        status
    }

    fn accept(&mut self, pipe: i32, bytes: &[u8], on_chunk: &mut Option<OnChunk>) {
        if let Some(f) = on_chunk {
            f(pipe, bytes);
            return;
        }
        let room = self.max_output.saturating_sub(self.out.len() + self.err.len());
        let target = if pipe == 1 { &mut self.out } else { &mut self.err };
        if bytes.len() > room {
            target.extend_from_slice(&bytes[..room]);
            self.truncated = true;
        } else {
            target.extend_from_slice(bytes);
        }
    }

    fn stop(&self) {
        unsafe { libc::kill(-self.pid, libc::SIGTERM) };
    }
}

/// The absolute path of [name] in [SYSTEM_ENV]'s PATH, or None.
pub fn which(name: &str) -> Option<String> {
    if name.starts_with('/') {
        return crate::os::fs::is_executable(name).then(|| name.to_string());
    }
    SYSTEM_ENV[0]
        .trim_start_matches("PATH=")
        .split(':')
        .map(|dir| format!("{dir}/{name}"))
        .find(|p| crate::os::fs::is_executable(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn separates_streams_and_feeds_stdin() {
        let r = run(&sh("cat; echo err >&2; exit 3"), Run { stdin: Some(b"hello\n".to_vec()), ..Default::default() })
            .unwrap();
        assert_eq!(r.exit_code, 3);
        assert_eq!(r.out(), "hello\n");
        assert_eq!(r.err(), "err\n");
    }

    #[test]
    fn the_environment_is_exactly_the_one_given() {
        let r = run(&sh("env"), Run { env: vec!["ONLY=this".into()], ..Default::default() }).unwrap();
        assert!(r.out().contains("ONLY=this"));
        assert!(!r.out().contains("HOME="));
    }

    #[test]
    fn arguments_never_reach_a_shell() {
        let r = run(&["/bin/echo".into(), "a; echo injected".into(), "$(id)".into()], Run::default()).unwrap();
        assert_eq!(r.out(), "a; echo injected $(id)\n");
    }

    #[test]
    fn a_timeout_stops_the_whole_group() {
        let start = Instant::now();
        let r = run(&sh("sleep 30 & echo $!; sleep 30"), Run { timeout: Duration::from_secs(1), ..Default::default() })
            .unwrap();
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
        // The grandchild too, not only the shell that started it: gone, or a zombie waiting for its new parent.
        let grandchild = r.out().trim().to_string();
        let alive = || {
            std::fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .ok()
                .and_then(|s| s.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next()))
                .is_some_and(|state| state != 'Z')
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "sleep {grandchild} outlived the timeout");
    }

    #[test]
    fn output_over_the_cap_is_cut() {
        let r = run(
            &sh("while :; do echo xxxxxxxxxxxxxxxx; done"),
            Run { max_output: 10_000, timeout: Duration::from_secs(10), ..Default::default() },
        )
        .unwrap();
        assert!(r.truncated);
        assert!(!r.timed_out);
        assert!(r.stdout.len() <= 10_000);
    }

    #[test]
    fn streams_when_asked() {
        let mut chunks = Vec::new();
        let mut collect = |fd: i32, bytes: &[u8]| chunks.push((fd, String::from_utf8_lossy(bytes).into_owned()));
        let r = run(&sh("echo one; echo two >&2"), Run { on_chunk: Some(&mut collect), ..Default::default() }).unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.is_empty());
        chunks.sort();
        assert_eq!(chunks, [(1, "one\n".to_string()), (2, "two\n".to_string())]);
    }

    #[test]
    fn a_child_that_ignores_stdin_does_not_kill_us() {
        let r = run(&["/bin/true".into()], Run { stdin: Some(vec![b'x'; 1_000_000]), ..Default::default() }).unwrap();
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn stdin_is_dev_null_every_time() {
        for i in 0..50 {
            let r = run(&sh("read x; echo eof:$?"), Run::default()).unwrap();
            assert_eq!(r.out(), "eof:1\n", "run {i}: exit {}", r.exit_code);
        }
    }

    #[test]
    fn which_finds_in_the_fixed_path() {
        assert_eq!(which("/bin/sh").as_deref(), Some("/bin/sh"));
        assert!(which("sh").unwrap().ends_with("/sh"));
        assert_eq!(which("no-such-program-limen"), None);
    }
}
