//! The hub: the MCP server over stdio or HTTP, its SSH client, and its directory (spec §2, §7.2, §9).

pub mod dir;
pub mod mcp;
pub mod server;
pub mod ssh;
pub mod transports;

use limen_core::protocol::{ErrorCode, LimenError, NodeError, NodeResponse, Result, error};
use serde_json::{Map, Value};
use std::time::Duration;

/// What the MCP server asks nodes through: the SSH client, or a fake in tests.
pub trait NodeClient: Send + Sync {
    /// The configured nodes; an error when the hub's configuration can't be read.
    fn nodes(&self) -> Result<Vec<String>>;

    /// One request to [node]. [timeout] overrides `[ssh].request_timeout`, for a check that declares a longer one.
    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse;
}

pub use limen_core::join::constant_time_eq;

/// The hub's own failure: a file it can't write, a program it can't start.
fn internal(message: impl Into<String>) -> LimenError {
    error(ErrorCode::Internal, message)
}

/// A node's error as the hub shows it.
fn failure_text(failure: &NodeError) -> String {
    format!("{}: {}", failure.code, failure.message)
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
