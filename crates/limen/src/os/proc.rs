//! Child processes without a shell (spec §12): an argument array, stdout and stderr on separate pipes read with a
//! cap, a timeout that stops the whole process group, and an environment that is exactly the one given.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
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

/// The process groups running now, for [stop_all].
static RUNNING: Mutex<Vec<Pid>> = Mutex::new(Vec::new());

/// Kills every process group still running: a request out of time leaves nothing behind.
pub fn stop_all() {
    for pid in RUNNING.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        kill_process_group(*pid, Signal::KILL).ok();
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
        pid: Pid::from_raw(child.id() as i32),
        out: Vec::new(),
        err: Vec::new(),
        truncated: false,
        timed_out: false,
        max_output: opts.max_output,
    };
    let pid = running.pid;
    let running_now = |add: bool| {
        let mut list = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        match pid {
            Some(pid) if add => list.push(pid),
            Some(pid) => list.retain(|p| *p != pid),
            None => {}
        }
    };
    running_now(true);
    let status = running.await_child(&mut child, opts.stdin.take(), opts.timeout, &mut opts.on_chunk);
    running_now(false);
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
    pid: Option<Pid>,
    out: Vec<u8>,
    err: Vec<u8>,
    truncated: bool,
    timed_out: bool,
    max_output: usize,
}

/// The ends of the child's pipes this process holds.
#[derive(Clone, Copy)]
enum End {
    Stdin,
    Stdout,
    Stderr,
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
        let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
        let input = input.unwrap_or_default();
        // Written as the child reads, never blocking: a child that doesn't read must not stop us reading its output.
        let mut stdin = child.stdin.take().filter(|_| !input.is_empty());
        if let Some(s) = &stdin {
            rustix::io::ioctl_fionbio(s, true).ok();
        }
        let mut written = 0;
        let mut status = None;
        // After the child exits, what a grandchild keeps open must not hold the answer: drain what is there and stop.
        let mut exited_at: Option<Instant> = None;
        let mut buffer = vec![0u8; CHUNK];
        let tick = Timespec { tv_sec: 0, tv_nsec: 50_000_000 };
        while stdout.is_some() || stderr.is_some() {
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
            if status.is_some() {
                let since = *exited_at.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(200) {
                    break;
                }
            }
            if start.elapsed() > timeout {
                self.timed_out = true;
                self.signal(Signal::TERM);
                break;
            }
            let mut ends = Vec::with_capacity(3);
            let mut fds = Vec::with_capacity(3);
            if let Some(o) = &stdout {
                ends.push(End::Stdout);
                fds.push(PollFd::new(o, PollFlags::IN));
            }
            if let Some(e) = &stderr {
                ends.push(End::Stderr);
                fds.push(PollFd::new(e, PollFlags::IN));
            }
            if let Some(i) = &stdin {
                ends.push(End::Stdin);
                fds.push(PollFd::new(i, PollFlags::OUT));
            }
            match poll(&mut fds, Some(&tick)) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => {
                    self.signal(Signal::TERM);
                    break;
                }
            }
            let ready: Vec<End> =
                ends.into_iter().zip(&fds).filter(|(_, fd)| !fd.revents().is_empty()).map(|(e, _)| e).collect();
            drop(fds);
            for end in ready {
                match end {
                    End::Stdin => {
                        // The child may close its stdin early: EPIPE is an answer, not a crash (SIGPIPE is ignored).
                        match stdin.as_mut().map(|s| s.write(&input[written..])) {
                            Some(Ok(n)) => written += n,
                            Some(Err(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                            _ => written = input.len(),
                        }
                        if written >= input.len() {
                            stdin = None;
                        }
                    }
                    End::Stdout => {
                        if let Some(n) = read_some(&mut stdout, &mut buffer) {
                            self.accept(end, &buffer[..n], on_chunk);
                        }
                    }
                    End::Stderr => {
                        if let Some(n) = read_some(&mut stderr, &mut buffer) {
                            self.accept(end, &buffer[..n], on_chunk);
                        }
                    }
                }
            }
            if self.truncated {
                self.signal(Signal::TERM);
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
                self.signal(Signal::KILL);
                status = child.wait().ok();
            }
        }
        status
    }

    fn accept(&mut self, end: End, bytes: &[u8], on_chunk: &mut Option<OnChunk>) {
        let fd = if matches!(end, End::Stdout) { 1 } else { 2 };
        if let Some(f) = on_chunk {
            f(fd, bytes);
            return;
        }
        let room = self.max_output.saturating_sub(self.out.len() + self.err.len());
        let target = if fd == 1 { &mut self.out } else { &mut self.err };
        if bytes.len() > room {
            target.extend_from_slice(&bytes[..room]);
            self.truncated = true;
        } else {
            target.extend_from_slice(bytes);
        }
    }

    /// To the child's whole process group, which it leads.
    fn signal(&self, signal: Signal) {
        if let Some(pid) = self.pid {
            kill_process_group(pid, signal).ok();
        }
    }
}

/// What a ready pipe has: the bytes read into [buffer], or None; at its end, or on an error, the pipe is dropped.
fn read_some(pipe: &mut Option<impl Read>, buffer: &mut [u8]) -> Option<usize> {
    match pipe.as_mut()?.read(buffer) {
        Ok(n) if n > 0 => Some(n),
        Err(e) if e.kind() == ErrorKind::Interrupted => None,
        _ => {
            *pipe = None;
            None
        }
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
