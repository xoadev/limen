//! [NodeClient] over the system `ssh` (spec §7.2, §12): batch mode, the configured key only, a `known_hosts` limen
//! writes from the pinned host keys, and connection multiplexing so a tool call does not pay a handshake.

use super::NodeClient;
use crate::os::{fs, proc, sys};
use limen_core::config::hub::{HubConfig, NodeEntry};
use limen_core::protocol::{ErrorCode, LimenError, NodeError, NodeRequest, NodeResponse, Result, error};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// At most N requests at once per node.
struct Semaphore {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore { free: Mutex::new(n), freed: Condvar::new() }
    }

    fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        let mut free = self.freed.wait_while(self.free.lock().unwrap(), |n| *n == 0).unwrap();
        *free -= 1;
        drop(free);
        let out = f();
        *self.free.lock().unwrap() += 1;
        self.freed.notify_one();
        out
    }
}

pub struct SshClient {
    config: HubConfig,
    ssh: String,
    runtime: String,
    known_hosts: String,
    identity: String,
    limits: BTreeMap<String, Semaphore>,
}

impl SshClient {
    pub fn new(config: HubConfig, home: &str) -> Result<SshClient> {
        let ssh = proc::which("ssh").ok_or_else(|| error(ErrorCode::Unavailable, "ssh is not installed on the hub"))?;
        let runtime = runtime_dir()?;
        let known_hosts = format!("{runtime}/known_hosts");
        let identity = if config.identity.starts_with('/') {
            config.identity.clone()
        } else {
            format!("{home}/{}", config.identity)
        };
        if fs::stat(&identity).map(|i| i.kind) != Some(fs::FileType::File) {
            return Err(error(
                ErrorCode::Unavailable,
                format!("no SSH key at {identity} ([ssh].identity in {home}/limen.toml)"),
            ));
        }
        fs::write_atomic(&known_hosts, known_hosts_text(&config.nodes).as_bytes(), 0o600)
            .map_err(|e| error(ErrorCode::Internal, e))?;
        let limits =
            config.nodes.iter().map(|n| (n.name.clone(), Semaphore::new(config.per_node_concurrency))).collect();
        Ok(SshClient { config, ssh, runtime, known_hosts, identity, limits })
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    /// Sends [line] and hands over the answer as it comes: for `limen call` of a deploy request, whose answer is the
    /// scripts' output as text.
    pub fn stream(
        &self,
        node: &str,
        user: &str,
        key: Option<&str>,
        line: &str,
        timeout: Duration,
        on_chunk: &mut dyn FnMut(i32, &[u8]),
    ) -> Result<proc::ProcResult> {
        let entry =
            self.config.node(node).ok_or_else(|| error(ErrorCode::BadRequest, format!("no node named '{node}'")))?;
        // Never through the multiplexed connection: on OpenWrt both roles log in as root, and a deploy request would
        // ride the socket the read key opened, landing in the read role.
        let argv = self.argv(entry, user, key.unwrap_or(&self.identity), false);
        proc::run(
            &argv,
            proc::Run {
                env: env(),
                stdin: Some(line.as_bytes().to_vec()),
                timeout,
                on_chunk: Some(on_chunk),
                ..Default::default()
            },
        )
        .map_err(|e| error(ErrorCode::Internal, e))
    }

    fn argv(&self, entry: &NodeEntry, user: &str, key: &str, multiplex: bool) -> Vec<String> {
        let mut options = vec![
            "BatchMode=yes".to_string(),
            "IdentitiesOnly=yes".into(),
            "StrictHostKeyChecking=yes".into(),
            format!("UserKnownHostsFile={}", self.known_hosts),
            "GlobalKnownHostsFile=/dev/null".into(),
            format!("HostKeyAlias={}", alias(entry)),
            format!("ConnectTimeout={}", self.config.connect_timeout.as_secs().max(1)),
            "ServerAliveInterval=15".into(),
            "LogLevel=ERROR".into(),
        ];
        if multiplex {
            options.extend([
                "ControlMaster=auto".into(),
                format!("ControlPath={}/cm-%C", self.runtime),
                "ControlPersist=60".into(),
            ]);
        } else {
            options.extend(["ControlMaster=no".into(), "ControlPath=none".into()]);
        }
        let mut argv = vec![self.ssh.clone(), "-T".into(), "-i".into(), key.into()];
        for o in options {
            argv.extend(["-o".into(), o]);
        }
        argv.extend(["-p".into(), entry.port.to_string(), "-l".into(), user.into(), "--".into(), entry.host.clone()]);
        argv
    }

    fn response(&self, node: &str, r: &proc::ProcResult, limit: Duration) -> NodeResponse {
        if r.timed_out {
            return failure(ErrorCode::Timeout, format!("{node} did not answer in {}s", limit.as_secs()));
        }
        let out = r.out();
        let out = out.trim();
        if r.exit_code == 255 || (out.is_empty() && r.exit_code != 0) {
            let err = r.err();
            let err = err.trim();
            if err.contains("Host key verification failed") || err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
                return failure(
                    ErrorCode::HostKeyMismatch,
                    format!("{node}'s host key does not match host_key in limen.toml"),
                );
            }
            let last = err
                .lines()
                .last()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .unwrap_or(format!("ssh exit {}", r.exit_code));
            return failure(ErrorCode::Unreachable, format!("{node}: {last}"));
        }
        serde_json::from_str(out.lines().last().unwrap_or("")).unwrap_or_else(|_| {
            let seen: String = format!("{out}{}", r.err()).chars().take(300).collect();
            failure(ErrorCode::Internal, format!("{node} answered something that is not limen's protocol: {seen}"))
        })
    }
}

impl NodeClient for SshClient {
    fn nodes(&self) -> Result<Vec<String>> {
        Ok(self.config.nodes.iter().map(|n| n.name.clone()).collect())
    }

    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse {
        let (Some(entry), Some(limit)) = (self.config.node(node), self.limits.get(node)) else {
            return NodeResponse::failure(&error(ErrorCode::BadRequest, format!("no node named '{node}'")));
        };
        let request = NodeRequest { v: 1, request: request.into(), args: args.clone() };
        let line = format!("{}\n", serde_json::to_string(&request).expect("a request serializes"));
        let timeout = timeout.unwrap_or(self.config.request_timeout);
        let argv = self.argv(entry, &entry.user, &self.identity, true);
        let result = limit.with(|| {
            proc::run(
                &argv,
                proc::Run {
                    env: env(),
                    stdin: Some(line.into_bytes()),
                    timeout,
                    max_output: 32 << 20,
                    ..Default::default()
                },
            )
        });
        match result {
            Ok(r) => self.response(node, &r, timeout),
            Err(e) => NodeResponse::failure(&LimenError::new(ErrorCode::Internal, e)),
        }
    }
}

fn failure(code: ErrorCode, message: String) -> NodeResponse {
    NodeResponse {
        ok: false,
        data: None,
        truncated: false,
        error: Some(NodeError { code: code.wire().into(), message, versions: None }),
    }
}

fn env() -> Vec<String> {
    let mut env = vec!["PATH=/usr/local/bin:/usr/bin:/bin".to_string(), "LANG=C.UTF-8".to_string()];
    for name in ["HOME", "USER", "SSH_AUTH_SOCK"] {
        if let Some(v) = sys::env(name) {
            env.push(format!("{name}={v}"));
        }
    }
    env
}

/// The name a node's key is filed under (`HostKeyAlias`): its limen name, whatever address it has today.
pub fn alias(entry: &NodeEntry) -> String {
    format!("limen-{}", entry.name)
}

pub fn known_hosts_text(nodes: &[NodeEntry]) -> String {
    nodes.iter().map(|n| format!("{} {}\n", alias(n), n.host_key)).collect()
}

/// `/tmp/limen-<uid>`, for the control sockets and `known_hosts`: short, because a Unix socket path has a limit of
/// 108 bytes, and refused if someone else owns it.
fn runtime_dir() -> Result<String> {
    let base = sys::env("XDG_RUNTIME_DIR").filter(|b| !b.is_empty() && b.len() < 40).unwrap_or_else(|| "/tmp".into());
    let dir = format!("{base}/limen-{}", sys::euid());
    fs::mkdirs(&dir, 0o700).map_err(|e| error(ErrorCode::Internal, e))?;
    match fs::lstat(&dir) {
        Some(i) if i.kind == fs::FileType::Directory && i.uid == sys::euid() && i.mode & 0o077 == 0 => Ok(dir),
        _ => Err(error(ErrorCode::Internal, format!("{dir} must be a directory of this user with mode 0700"))),
    }
}
