//! The node side: what `limen gate` answers, and how `install` and `join` set a machine up.

pub mod files;
pub mod gate;
pub mod installer;
pub mod joiner;
pub mod lint;
pub mod requests;
pub mod scripts;

use crate::os::{fs, sys};
use limen_core::config::node::NodeConfig;
use limen_core::path_policy::PathPolicy;
use limen_core::protocol::Result;
use limen_core::redactor::Redactor;
use serde_json::Value;

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
        let redactor = Redactor::new(&config.redact_names, &config.redact_patterns);
        let policy = PathPolicy::new(&config.allow, &config.deny).with_private(config.private_paths());
        Node { config, redactor, policy, trusted_owner: sys::euid() }
    }

    /// The node's configuration; none means nothing is readable and no script offered. A broken one, or one that
    /// someone other than root could have written, is `internal`, with the reason.
    pub fn load(path: &str) -> Result<Node> {
        let Some(real) = fs::real_path(path) else { return Ok(Node::new(NodeConfig::default())) };
        let unusable = |reason: String| internal(format!("{path}: {reason}"));
        let text = read_if_trusted(&real, sys::euid()).map_err(unusable)?;
        NodeConfig::parse(&text).map(Node::new).map_err(|reason| unusable(reason.to_string()))
    }

    /// The last [tail] lines of [text] (at most `limits.max_lines`) that hold [grep], case aside; and whether a limit
    /// cut them. [text] is redacted already: matching before redaction would tell a guess at a secret apart, one
    /// character at a time, by whether a line comes back.
    pub fn filter(&self, text: &str, grep: Option<&str>, tail: Option<usize>) -> (Vec<String>, bool) {
        let limit = tail.unwrap_or(self.config.max_lines).min(self.config.max_lines);
        let lines: Vec<&str> = text.lines().collect();
        let matched = last_matching(lines, grep, usize::MAX);
        let cut = matched.len() > limit && tail.is_none_or(|tail| tail > limit);
        let kept = matched[matched.len().saturating_sub(limit)..].iter().map(ToString::to_string).collect();
        (kept, cut)
    }
}

/// The last [count] of [lines] that hold [grep], case aside.
pub fn last_matching<'a>(lines: Vec<&'a str>, grep: Option<&str>, count: usize) -> Vec<&'a str> {
    let grep = grep.map(str::to_lowercase);
    let mut matched: Vec<&str> =
        lines.into_iter().filter(|line| grep.as_ref().is_none_or(|grep| line.to_lowercase().contains(grep))).collect();
    let first_kept = matched.len().saturating_sub(count);
    matched.split_off(first_kept)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_keep_the_last_matching_lines_within_the_limit() {
        let node = Node::new(NodeConfig { max_lines: 3, ..Default::default() });
        let text = "a1\nb\nA2\na3\na4\na5";
        assert_eq!(node.filter(text, Some("a"), Some(2)), (vec!["a4".into(), "a5".into()], false));
        assert_eq!(node.filter(text, Some("a"), None), (vec!["a3".into(), "a4".into(), "a5".into()], true));
        assert!(node.filter(text, None, Some(10)).1, "more than max_lines asked, and cut");
        assert_eq!(node.filter("x\ny", None, None), (vec!["x".into(), "y".into()], false));
    }
}
