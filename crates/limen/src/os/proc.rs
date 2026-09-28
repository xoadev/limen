//! Child processes without a shell (spec §12): an argument array, stdout and stderr on separate pipes read with a
//! cap, a timeout that stops the whole process group, and an environment that is exactly the one given.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;
/// How long a child told to stop has before its group is killed.
const KILL_GRACE: Duration = Duration::from_secs(2);
const REAP_INTERVAL: Duration = Duration::from_millis(20);
/// How long output is still read after the child exits.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(200);
/// How long one wait on the pipes lasts, so the timeout and the child's exit are looked at between waits.
const POLL_WAIT: Timespec = Timespec { tv_sec: 0, tv_nsec: 50_000_000 };

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
    let mut env = system_env();
    env.push("HOME=/root".into());
    env
}

pub fn system_env() -> Vec<String> {
    SYSTEM_ENV.iter().map(|variable| (*variable).to_string()).collect()
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

    /// Why it failed: the last line of its stderr, or its exit code when it wrote nothing there.
    pub fn failure_reason(&self) -> String {
        self.err().trim().lines().last().map_or_else(|| format!("exit {}", self.exit_code), String::from)
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

// A poisoned list is still the list of what to kill.
fn lock_running() -> MutexGuard<'static, Vec<Pid>> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kills every process group still running: a request out of time leaves nothing behind.
pub fn stop_all() {
    for pid in lock_running().iter() {
        kill_process_group(*pid, Signal::KILL).ok();
    }
}

/// Runs [argv] to completion. `argv[0]` is an absolute path (see [which]).
pub fn run(argv: &[String], mut options: Run) -> Result<ProcResult, String> {
    let program = argv
        .first()
        .filter(|program| program.starts_with('/'))
        .ok_or_else(|| format!("argv[0] must be an absolute path: {argv:?}"))?;
    let mut child = spawn(program, &argv[1..], &options.env, options.stdin.is_some())?;
    let mut running = Running {
        pid: Pid::from_raw(child.id() as i32),
        stdout: Vec::new(),
        stderr: Vec::new(),
        truncated: false,
        timed_out: false,
        max_output: options.max_output,
    };
    track(running.pid);
    let status = running.await_child(&mut child, options.stdin.take(), options.timeout, &mut options.on_chunk);
    untrack(running.pid);
    let (exit_code, signal) = exit_code_and_signal(status);
    Ok(ProcResult {
        exit_code,
        signal,
        stdout: running.stdout,
        stderr: running.stderr,
        timed_out: running.timed_out,
        truncated: running.truncated,
    })
}

/// [program] with [args] and exactly [env], its output on pipes and its stdin a pipe only [with_stdin].
fn spawn(program: &str, args: &[String], env: &[String], with_stdin: bool) -> Result<Child, String> {
    Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().filter_map(|variable| variable.split_once('=')))
        .stdin(if with_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout reaches what a script started too.
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))
}

fn track(pid: Option<Pid>) {
    if let Some(pid) = pid {
        lock_running().push(pid);
    }
}

fn untrack(pid: Option<Pid>) {
    if let Some(pid) = pid {
        lock_running().retain(|running| *running != pid);
    }
}

/// The exit code, or -1 and the signal that ended the process; -1 alone when its end is unknown.
fn exit_code_and_signal(status: Option<ExitStatus>) -> (i32, Option<i32>) {
    let Some(status) = status else { return (-1, None) };
    match status.code() {
        Some(code) => (code, None),
        None => (-1, status.signal()),
    }
}

/// A child being run, and what it has written so far.
struct Running {
    pid: Option<Pid>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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

/// This process's ends of the child's pipes while they are open, and the input still to write.
struct Pipes {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    input: Vec<u8>,
    written: usize,
}

impl Pipes {
    fn take(child: &mut Child, input: Option<Vec<u8>>) -> Self {
        let input = input.unwrap_or_default();
        // Written as the child reads, never blocking: a child that doesn't read must not stop us reading its output.
        let stdin = child.stdin.take().filter(|_| !input.is_empty());
        if let Some(stdin) = &stdin {
            rustix::io::ioctl_fionbio(stdin, true).ok();
        }
        Pipes { stdin, stdout: child.stdout.take(), stderr: child.stderr.take(), input, written: 0 }
    }

    fn output_open(&self) -> bool {
        self.stdout.is_some() || self.stderr.is_some()
    }

    /// The ends ready within [wait].
    fn poll(&self, wait: &Timespec) -> rustix::io::Result<Vec<End>> {
        let mut ends = Vec::with_capacity(3);
        let mut fds = Vec::with_capacity(3);
        if let Some(stdout) = &self.stdout {
            ends.push(End::Stdout);
            fds.push(PollFd::new(stdout, PollFlags::IN));
        }
        if let Some(stderr) = &self.stderr {
            ends.push(End::Stderr);
            fds.push(PollFd::new(stderr, PollFlags::IN));
        }
        if let Some(stdin) = &self.stdin {
            ends.push(End::Stdin);
            fds.push(PollFd::new(stdin, PollFlags::OUT));
        }
        poll(&mut fds, Some(wait))?;
        Ok(ends.into_iter().zip(&fds).filter(|(_, fd)| !fd.revents().is_empty()).map(|(end, _)| end).collect())
    }

    /// Writes what the child's stdin takes now, and closes it once all the input is written.
    fn feed(&mut self) {
        // The child may close its stdin early: EPIPE is an answer, not a crash (SIGPIPE is ignored).
        match self.stdin.as_mut().map(|stdin| stdin.write(&self.input[self.written..])) {
            Some(Ok(n)) => self.written += n,
            Some(Err(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            _ => self.written = self.input.len(),
        }
        if self.written >= self.input.len() {
            self.stdin = None;
        }
    }
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
        let mut pipes = Pipes::take(child, input);
        let mut status = None;
        // After the child exits, what a grandchild keeps open must not hold the answer: drain what is there and stop.
        let mut exited_at: Option<Instant> = None;
        let mut buffer = vec![0u8; CHUNK];
        while pipes.output_open() {
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
            if status.is_some() && exited_at.get_or_insert_with(Instant::now).elapsed() > DRAIN_AFTER_EXIT {
                break;
            }
            if start.elapsed() > timeout {
                self.timed_out = true;
                self.signal(Signal::TERM);
                break;
            }
            let ready = match pipes.poll(&POLL_WAIT) {
                Ok(ready) => ready,
                Err(Errno::INTR) => continue,
                Err(_) => {
                    self.signal(Signal::TERM);
                    break;
                }
            };
            for end in ready {
                self.transfer(&mut pipes, end, &mut buffer, on_chunk);
            }
            if self.truncated {
                self.signal(Signal::TERM);
                break;
            }
        }
        drop(pipes);
        status.or_else(|| self.reap(child))
    }

    /// Moves what [end] is ready for: input into the child, or its output into this run.
    fn transfer(&mut self, pipes: &mut Pipes, end: End, buffer: &mut [u8], on_chunk: &mut Option<OnChunk>) {
        let read = match end {
            End::Stdin => return pipes.feed(),
            End::Stdout => read_some(&mut pipes.stdout, buffer),
            End::Stderr => read_some(&mut pipes.stderr, buffer),
        };
        if let Some(n) = read {
            self.accept(end, &buffer[..n], on_chunk);
        }
    }

    /// The pipes closed or the child was told to stop: it gets [KILL_GRACE] to exit, then its group is killed.
    fn reap(&self, child: &mut Child) -> Option<ExitStatus> {
        let deadline = Instant::now() + KILL_GRACE;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().ok().flatten() {
                return Some(status);
            }
            std::thread::sleep(REAP_INTERVAL);
        }
        self.signal(Signal::KILL);
        child.wait().ok()
    }

    fn accept(&mut self, end: End, bytes: &[u8], on_chunk: &mut Option<OnChunk>) {
        let fd = if matches!(end, End::Stdout) { 1 } else { 2 };
        if let Some(stream) = on_chunk {
            stream(fd, bytes);
            return;
        }
        let room = self.max_output.saturating_sub(self.stdout.len() + self.stderr.len());
        let kept = if fd == 1 { &mut self.stdout } else { &mut self.stderr };
        if bytes.len() > room {
            kept.extend_from_slice(&bytes[..room]);
            self.truncated = true;
        } else {
            kept.extend_from_slice(bytes);
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
        Err(error) if error.kind() == ErrorKind::Interrupted => None,
        _ => {
            *pipe = None;
            None
        }
    }
}

/// The absolute path of [name] in [SYSTEM_ENV]'s PATH, or None.
/// [argv] with its program found as [which] finds it; None when the program is not installed.
pub fn located(argv: &[&str]) -> Option<Vec<String>> {
    let (program, arguments) = argv.split_first()?;
    Some(std::iter::once(which(program)?).chain(arguments.iter().map(ToString::to_string)).collect())
}

pub fn which(name: &str) -> Option<String> {
    if name.starts_with('/') {
        return crate::os::fs::is_executable(name).then(|| name.to_string());
    }
    SYSTEM_ENV[0]
        .trim_start_matches("PATH=")
        .split(':')
        .map(|dir| format!("{dir}/{name}"))
        .find(|candidate| crate::os::fs::is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn separates_streams_and_feeds_stdin() {
        let result =
            run(&sh("cat; echo err >&2; exit 3"), Run { stdin: Some(b"hello\n".to_vec()), ..Default::default() })
                .unwrap();
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.out(), "hello\n");
        assert_eq!(result.err(), "err\n");
    }

    #[test]
    fn the_environment_is_exactly_the_one_given() {
        let result = run(&sh("env"), Run { env: vec!["ONLY=this".into()], ..Default::default() }).unwrap();
        assert!(result.out().contains("ONLY=this"));
        assert!(!result.out().contains("HOME="));
    }

    #[test]
    fn arguments_never_reach_a_shell() {
        let result = run(&["/bin/echo".into(), "a; echo injected".into(), "$(id)".into()], Run::default()).unwrap();
        assert_eq!(result.out(), "a; echo injected $(id)\n");
    }

    #[test]
    fn a_timeout_stops_the_whole_group() {
        let start = Instant::now();
        let result =
            run(&sh("sleep 30 & echo $!; sleep 30"), Run { timeout: Duration::from_secs(1), ..Default::default() })
                .unwrap();
        assert!(result.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));
        // The grandchild too, not only the shell that started it: gone, or a zombie waiting for its new parent.
        let grandchild = result.out().trim().to_string();
        let alive = || {
            std::fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next()))
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
        let result = run(
            &sh("while :; do echo xxxxxxxxxxxxxxxx; done"),
            Run { max_output: 10_000, timeout: Duration::from_secs(10), ..Default::default() },
        )
        .unwrap();
        assert!(result.truncated);
        assert!(!result.timed_out);
        assert!(result.stdout.len() <= 10_000);
    }

    #[test]
    fn streams_when_asked() {
        let mut chunks = Vec::new();
        let mut collect = |fd: i32, bytes: &[u8]| chunks.push((fd, String::from_utf8_lossy(bytes).into_owned()));
        let result =
            run(&sh("echo one; echo two >&2"), Run { on_chunk: Some(&mut collect), ..Default::default() }).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.is_empty());
        chunks.sort();
        assert_eq!(chunks, [(1, "one\n".to_string()), (2, "two\n".to_string())]);
    }

    #[test]
    fn a_child_that_ignores_stdin_does_not_kill_us() {
        let result =
            run(&["/bin/true".into()], Run { stdin: Some(vec![b'x'; 1_000_000]), ..Default::default() }).unwrap();
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn stdin_is_dev_null_every_time() {
        for i in 0..50 {
            let result = run(&sh("read x; echo eof:$?"), Run::default()).unwrap();
            assert_eq!(result.out(), "eof:1\n", "run {i}: exit {}", result.exit_code);
        }
    }

    #[test]
    fn which_finds_in_the_fixed_path() {
        assert_eq!(which("/bin/sh").as_deref(), Some("/bin/sh"));
        assert!(which("sh").unwrap().ends_with("/sh"));
        assert_eq!(which("no-such-program-limen"), None);
    }
}
