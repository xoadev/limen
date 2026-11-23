//! The hub: the MCP server over stdio or HTTP, its SSH client, and its directory (spec §2, §7.2, §9).

pub mod dir;
pub mod mcp;
pub mod mqtt;
pub mod server;
pub mod ssh;
pub mod transports;

use limen_core::config::hub::Approval;
use limen_core::protocol::{Chunk, NodeResponse, Result};
use serde_json::{Map, Value};
use std::time::Duration;

/// What the MCP server asks nodes through: the SSH client, or a fake in tests.
pub trait NodeClient: Send + Sync {
    /// The configured nodes; an error when the hub's configuration can't be read.
    fn nodes(&self) -> Result<Vec<String>>;

    /// One request to [node]. [timeout] overrides `[ssh].request_timeout`, for a check that declares a longer one.
    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse;

    /// A `run` with `follow` (spec §4): each chunk of the script's output to [on_chunk] as the node sends it, then the
    /// answer. One that has `stdout` came all at the end —from a node that predates `follow`, or a client that doesn't
    /// stream—, and [on_chunk] got nothing.
    fn follow(
        &self,
        node: &str,
        args: &Map<String, Value>,
        timeout: Option<Duration>,
        _on_chunk: &mut dyn FnMut(Chunk),
    ) -> NodeResponse {
        self.call(node, "run", args, timeout)
    }

    /// Which scripts a person approves before they run (`[approval]`), as the hub's configuration says now.
    fn approval(&self) -> Result<Approval>;
}

pub use limen_core::join::constant_time_eq;
use limen_core::protocol::internal;

#[cfg(test)]
mod tests {
    #[test]
    fn token_comparison() {
        assert!(super::constant_time_eq("abc", "abc"));
        assert!(!super::constant_time_eq("abc", "abd")); // typos: ignore-line
        assert!(!super::constant_time_eq("abc", "abcd"));
        assert!(!super::constant_time_eq("", "a"));
    }
}
