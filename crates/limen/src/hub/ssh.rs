//! [NodeClient] over the system `ssh` (spec §7.2, §12): batch mode, the configured key only, a `known_hosts` limen
//! writes from the pinned host keys, and connection multiplexing so a tool call does not pay a handshake.

use super::{NodeClient, internal};
use crate::os::{fs, proc, sys};
use limen_core::config::hub::{HubConfig, NodeEntry};
use limen_core::join;
use limen_core::protocol::{ErrorCode, LimenError, NodeRequest, NodeResponse, Result, error};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// What ssh may print for one of the hub's own requests.
const MAX_OUTPUT: usize = 32 << 20;

/// At most N requests at once per node.
struct Semaphore {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Semaphore {
    const UNPOISONED: &str = "nothing panics holding a semaphore";

    fn new(permits: usize) -> Self {
        Semaphore { free: Mutex::new(permits), freed: Condvar::new() }
    }

    /// Runs [work] once a permit is free, and frees it after.
    fn with<T>(&self, work: impl FnOnce() -> T) -> T {
        let lock = self.free.lock().expect(Self::UNPOISONED);
        let mut free = self.freed.wait_while(lock, |free| *free == 0).expect(Self::UNPOISONED);
        *free -= 1;
        drop(free);
        let result = work();
        *self.free.lock().expect(Self::UNPOISONED) += 1;
        self.freed.notify_one();
        result
    }
}

pub struct SshClient {
    config: HubConfig,
    ssh: String,
    runtime_dir: String,
    known_hosts: String,
    identity: String,
    per_node_limits: BTreeMap<String, Semaphore>,
}

impl SshClient {
    pub fn new(config: HubConfig, home: &str) -> Result<SshClient> {
        let ssh = proc::which("ssh").ok_or_else(|| error(ErrorCode::Unavailable, "ssh is not installed on the hub"))?;
        let runtime_dir = runtime_dir(home)?;
        // In the hub's own directory: two hubs of one user must not check their nodes against each other's keys.
        let known_hosts = format!("{home}/known_hosts");
        let identity = identity_file(&config, home)?;
        fs::write_atomic(&known_hosts, known_hosts_text(&config.nodes).as_bytes(), 0o600).map_err(internal)?;
        let per_node_limits =
            config.nodes.iter().map(|node| (node.name.clone(), Semaphore::new(config.per_node_concurrency))).collect();
        Ok(SshClient { config, ssh, runtime_dir, known_hosts, identity, per_node_limits })
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    fn argv(&self, entry: &NodeEntry) -> Vec<String> {
        // `-F none`: the operator's ~/.ssh/config, with its ProxyCommand or ForwardAgent, doesn't apply to the hub.
        let mut argv =
            vec![self.ssh.clone(), "-F".into(), "none".into(), "-T".into(), "-i".into(), self.identity.clone()];
        for option in self.options(entry) {
            argv.extend(["-o".into(), option]);
        }
        argv.extend([
            "-p".into(),
            entry.port.to_string(),
            "-l".into(),
            entry.user.clone(),
            "--".into(),
            entry.host.clone(),
        ]);
        argv
    }

    fn options(&self, entry: &NodeEntry) -> Vec<String> {
        vec![
            // Nothing forwarded to a node, which may be hostile: not the operator's agent, not a port.
            "ForwardAgent=no".to_string(),
            "ForwardX11=no".into(),
            "ClearAllForwardings=yes".into(),
            "BatchMode=yes".into(),
            "IdentitiesOnly=yes".into(),
            "StrictHostKeyChecking=yes".into(),
            format!("UserKnownHostsFile={}", self.known_hosts),
            "GlobalKnownHostsFile=/dev/null".into(),
            format!("HostKeyAlias={}", alias(entry)),
            format!("ConnectTimeout={}", self.config.connect_timeout_seconds()),
            "ServerAliveInterval=15".into(),
            "LogLevel=ERROR".into(),
            "ControlMaster=auto".into(),
            format!("ControlPath={}/cm-%C", self.runtime_dir),
            "ControlPersist=60".into(),
        ]
    }
}

impl NodeClient for SshClient {
    fn nodes(&self) -> Result<Vec<String>> {
        Ok(self.config.nodes.iter().map(|node| node.name.clone()).collect())
    }

    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse {
        let (Some(entry), Some(limit)) = (self.config.node(node), self.per_node_limits.get(node)) else {
            return NodeResponse::failure(&no_node(node));
        };
        let timeout = timeout.unwrap_or(self.config.request_timeout);
        let argv = self.argv(entry);
        let run = proc::Run {
            env: ssh_env(),
            stdin: Some(NodeRequest::new(request, args.clone()).to_line().into_bytes()),
            timeout,
            max_output: MAX_OUTPUT,
        };
        match limit.with(|| proc::run(&argv, run)) {
            Ok(result) => node_response(node, &result, timeout),
            Err(cause) => NodeResponse::failure(&internal(cause)),
        }
    }
}

fn no_node(node: &str) -> LimenError {
    error(ErrorCode::BadRequest, format!("no node named '{node}'"))
}

/// What ssh's run of a request means: the node's answer, the last line it printed, or why there was none.
fn node_response(node: &str, result: &proc::ProcResult, timeout: Duration) -> NodeResponse {
    if result.timed_out {
        return failure(ErrorCode::Timeout, format!("{node} did not answer in {}s", timeout.as_secs()));
    }
    let stdout = result.out();
    let stdout = stdout.trim();
    // 255 is ssh's own error; nothing printed and a failure is a node that never got to answer.
    if result.exit_code == 255 || (stdout.is_empty() && result.exit_code != 0) {
        return connection_failure(node, result);
    }
    serde_json::from_str(stdout.lines().last().unwrap_or("")).unwrap_or_else(|_| {
        let seen: String = format!("{stdout}{}", result.err()).chars().take(300).collect();
        failure(ErrorCode::Internal, format!("{node} answered something that is not limen's protocol: {seen}"))
    })
}

/// Why ssh did not get a request to the node: a host key that doesn't match, or ssh's last word.
fn connection_failure(node: &str, result: &proc::ProcResult) -> NodeResponse {
    let stderr = result.err();
    let stderr = stderr.trim();
    if stderr.contains("Host key verification failed") || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return failure(ErrorCode::HostKeyMismatch, format!("{node}'s host key does not match host_key in limen.toml"));
    }
    let last_line = stderr
        .lines()
        .last()
        .filter(|line| !line.is_empty())
        .map_or_else(|| format!("ssh exit {}", result.exit_code), String::from);
    failure(ErrorCode::Unreachable, format!("{node}: {last_line}"))
}

fn failure(code: ErrorCode, message: String) -> NodeResponse {
    NodeResponse::failure(&error(code, message))
}

/// ssh's environment: what it needs to run, and never the operator's agent.
fn ssh_env() -> Vec<String> {
    let mut env = vec!["PATH=/usr/local/bin:/usr/bin:/bin".to_string(), "LANG=C.UTF-8".to_string()];
    for name in ["HOME", "USER"] {
        if let Some(value) = sys::env(name) {
            env.push(format!("{name}={value}"));
        }
    }
    env
}

/// `[ssh].identity`, under the hub's directory unless absolute; an error when there is no key there.
fn identity_file(config: &HubConfig, home: &str) -> Result<String> {
    let identity = config.identity_path(home);
    if fs::stat(&identity).map(|info| info.kind) != Some(fs::FileType::File) {
        return Err(error(
            ErrorCode::Unavailable,
            format!("no SSH key at {identity} ([ssh].identity in {home}/limen.toml)"),
        ));
    }
    Ok(identity)
}

/// The name a node's key is filed under (`HostKeyAlias`): its limen name, whatever address it has today.
pub fn alias(entry: &NodeEntry) -> String {
    format!("limen-{}", entry.name)
}

pub fn known_hosts_text(nodes: &[NodeEntry]) -> String {
    nodes.iter().map(|entry| format!("{} {}\n", alias(entry), entry.host_key)).collect()
}

/// `$XDG_RUNTIME_DIR/limen-<uid>/<hub>`, or under `/tmp` when that is too long, for the control sockets: a Unix
/// socket path has a limit of 108 bytes, and ssh adds `/cm-`, 40 characters of `%C` and 17 of its own to it. Refused
/// if someone else owns it, and one per hub directory, so two hubs never share a connection.
fn runtime_dir(home: &str) -> Result<String> {
    const MAX_DIR: usize = 107 - 4 - 40 - 17;
    let hub_id = &join::hex(&join::sha256(home.as_bytes()))[..8];
    let user_dir_under = |base: &str| format!("{base}/limen-{}", sys::euid());
    let user_dir = sys::env("XDG_RUNTIME_DIR")
        .filter(|base| base.starts_with('/'))
        .map(|base| user_dir_under(&base))
        .filter(|user_dir| user_dir.len() + 1 + hub_id.len() <= MAX_DIR)
        .unwrap_or_else(|| user_dir_under("/tmp"));
    let dir = format!("{user_dir}/{hub_id}");
    fs::mkdirs(&dir, 0o700).map_err(internal)?;
    for path in [&user_dir, &dir] {
        require_private_dir(path)?;
    }
    Ok(dir)
}

fn require_private_dir(path: &str) -> Result<()> {
    match fs::lstat(path) {
        Some(info) if info.kind == fs::FileType::Directory && info.uid == sys::euid() && info.mode & 0o077 == 0 => {
            Ok(())
        }
        _ => Err(internal(format!("{path} must be a directory of this user with mode 0700"))),
    }
}
