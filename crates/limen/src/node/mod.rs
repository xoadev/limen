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
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::redactor::Redactor;
use serde_json::Value;
use std::time::Duration;

/// What `exec` keeps of each stream a program writes.
const MAX_OUTPUT_BYTES: usize = 16 << 20;
const EXEC_OK_TIMEOUT: Duration = Duration::from_secs(30);
const MINUTE: Duration = Duration::from_secs(60);

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

    /// The node's configuration; none means nothing is readable. A broken one, or one that someone other than root
    /// could have written, is `internal`, with the reason.
    pub fn load(path: &str) -> Result<Node> {
        let Some(real) = fs::real_path(path) else { return Ok(Node::new(NodeConfig::default())) };
        let unusable = |reason: String| internal(format!("{path}: {reason}"));
        let text = read_if_trusted(&real, sys::euid()).map_err(unusable)?;
        NodeConfig::parse(&text).map(Node::new).map_err(|reason| unusable(reason.to_string()))
    }

    /// [path], if only root —or the user limen runs as— could have written it (see [limen_core::trust]).
    pub fn trusted_text(&self, path: &str) -> Result<String> {
        read_if_trusted(path, self.trusted_owner).map_err(internal)
    }

    pub fn now(&self) -> i64 {
        limen_core::time::now()
    }

    /// Runs a system program by name. A program that is not there is `unavailable` (no Docker on this node), not an
    /// internal error.
    pub fn exec(&self, argv: &[&str], timeout: Duration) -> Result<proc::ProcResult> {
        let program = argv[0];
        let located = proc::located(argv)
            .ok_or_else(|| error(ErrorCode::Unavailable, format!("{program} is not installed on this node")))?;
        let options = proc::Run { env: proc::root_env(), timeout, max_output: MAX_OUTPUT_BYTES, ..Default::default() };
        let result = proc::run(&located, options).map_err(internal)?;
        if result.timed_out {
            return Err(error(ErrorCode::Timeout, format!("{program} did not finish in {}s", timeout.as_secs())));
        }
        Ok(result)
    }

    /// [exec] that must succeed; its stderr becomes the error.
    pub fn exec_ok(&self, argv: &[&str]) -> Result<String> {
        let result = self.exec(argv, EXEC_OK_TIMEOUT)?;
        if result.exit_code != 0 {
            return Err(internal(format!("{}: {}", argv[0], result.failure_reason())));
        }
        Ok(result.out())
    }
}

fn read_if_trusted(path: &str, owner: u32) -> std::result::Result<String, String> {
    let real = fs::real_path(path).ok_or_else(|| format!("{path} does not exist"))?;
    if let Some(why) = limen_core::trust::untrusted(&fs::chain(&real), owner) {
        return Err(format!("not trusted: {why}"));
    }
    fs::read_text(&real).ok_or_else(|| format!("cannot read {real}"))
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

pub use limen_core::protocol::internal;
