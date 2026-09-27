//! The node side: what `limen gate` answers, and how `install` and `join` set a machine up.

pub mod deploy;
pub mod gate;
pub mod installer;
pub mod joiner;
pub mod lint;
pub mod read;
pub mod repo;
pub mod scripts;
pub mod state;
pub mod system;

use crate::os::{fs, proc, sys};
use limen_core::config::node::NodeConfig;
use limen_core::path_policy::PathPolicy;
use limen_core::protocol::{ErrorCode, LimenError, Result, error};
use limen_core::redactor::Redactor;
use serde_json::Value;
use std::time::Duration;

/// Everything a request on this node is answered with: its configuration and what derives from it.
pub struct Node {
    pub config: NodeConfig,
    pub redactor: Redactor,
    pub policy: PathPolicy,
    /// Scripts must belong to the user limen runs as: root in production (spec §6).
    pub trusted_owner: u32,
}

impl Node {
    pub fn new(config: NodeConfig) -> Self {
        let redactor = Redactor::new(&config.redact);
        let policy = PathPolicy::new(&config.allow, &config.deny).with_private(config.private_paths());
        Node { config, redactor, policy, trusted_owner: sys::euid() }
    }

    /// The node's configuration; none means nothing is readable. A broken one is `internal`, with the reason.
    pub fn load(path: &str) -> Result<Node> {
        match fs::read_following(path) {
            None => Ok(Node::new(NodeConfig::default())),
            Some(text) => {
                NodeConfig::parse(&text).map(Node::new).map_err(|e| error(ErrorCode::Internal, format!("{path}: {e}")))
            }
        }
    }

    pub fn now(&self) -> i64 {
        limen_core::time::now()
    }

    /// Runs a system program by name. A program that is not there is `unavailable` (no Docker on this node), not an
    /// internal error.
    pub fn exec(&self, argv: &[&str], timeout: Duration) -> Result<proc::ProcResult> {
        self.exec_capped(argv, timeout, 16 << 20)
    }

    pub fn exec_capped(&self, argv: &[&str], timeout: Duration, max_output: usize) -> Result<proc::ProcResult> {
        let path = proc::which(argv[0])
            .ok_or_else(|| error(ErrorCode::Unavailable, format!("{} is not installed on this node", argv[0])))?;
        let full: Vec<String> = std::iter::once(path).chain(argv[1..].iter().map(|s| s.to_string())).collect();
        let r = proc::run(&full, proc::Run { env: proc::root_env(), timeout, max_output, ..Default::default() })
            .map_err(|e| error(ErrorCode::Internal, e))?;
        if r.timed_out {
            return Err(error(ErrorCode::Timeout, format!("{} did not finish in {}s", argv[0], timeout.as_secs())));
        }
        Ok(r)
    }

    /// [exec] that must succeed; its stderr becomes the error.
    pub fn exec_ok(&self, argv: &[&str]) -> Result<String> {
        let r = self.exec(argv, Duration::from_secs(30))?;
        if r.exit_code != 0 {
            let err = r.err();
            let last = err.trim().lines().last().map(String::from).unwrap_or_else(|| format!("exit {}", r.exit_code));
            return Err(error(ErrorCode::Internal, format!("{}: {last}", argv[0])));
        }
        Ok(r.out())
    }
}

/// What a request handler returns: the data, and whether a limit cut it.
pub struct Answer {
    pub data: Value,
    pub truncated: bool,
}

impl Answer {
    pub fn of(data: Value) -> Self {
        Answer { data, truncated: false }
    }

    pub fn cut(data: Value, truncated: bool) -> Self {
        Answer { data, truncated }
    }
}

pub fn internal(message: impl Into<String>) -> LimenError {
    error(ErrorCode::Internal, message)
}
