//! The hub: the MCP server over stdio or HTTP, its SSH client, and its directory (spec §2, §7.2, §9).

pub mod dir;
pub mod mcp;
pub mod server;
pub mod ssh;
pub mod transports;

use limen_core::protocol::{NodeResponse, Result};
use serde_json::{Map, Value};
use std::time::Duration;

/// What the MCP server asks nodes through: the SSH client, or a fake in tests.
pub trait NodeClient: Send + Sync {
    /// The configured nodes; an error when the hub's configuration can't be read.
    fn nodes(&self) -> Result<Vec<String>>;

    /// One request to [node]. [timeout] overrides `[ssh].request_timeout`, for a check that declares a longer one.
    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse;
}

/// Equal strings, compared in time that depends only on their length: for the HTTP token and the join proof.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_comparison() {
        assert!(super::constant_time_eq("abc", "abc"));
        assert!(!super::constant_time_eq("abc", "abd"));
        assert!(!super::constant_time_eq("abc", "abcd"));
        assert!(!super::constant_time_eq("", "a"));
    }
}
