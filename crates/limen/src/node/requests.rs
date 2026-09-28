//! What the gate answers (spec §4): `hello` and `history` here, files and scripts in their own modules.

use super::{Answer, Node, files, internal, scripts};
use crate::os::{fs, sys};
use limen_core::etc;
use limen_core::params::ArgsExt;
use limen_core::protocol::{PROTOCOL_VERSIONS, Result, bad_request};
use limen_core::version::VERSION;
use serde_json::{Map, Value, json};

type Args = Map<String, Value>;

/// What `history` reads per audit line it answers.
const AUDIT_BYTES_PER_LINE: u64 = 8192;

pub fn answer(node: &Node, name: &str, args: &Args) -> Result<Answer> {
    match name {
        "hello" => Ok(hello(node)),
        "read_file" => files::read_file(node, args),
        "list_dir" => files::list_dir(node, args),
        "history" => history(node, args),
        "run" => scripts::run(node, args),
        other => Err(bad_request(format!("unknown request '{other}'"))),
    }
}

pub fn hello(node: &Node) -> Answer {
    let (kernel, arch) = sys::uname();
    Answer::of(json!({
        "version": VERSION,
        "protocols": PROTOCOL_VERSIONS,
        "hostname": sys::hostname(),
        "os": fs::read_following("/etc/os-release").and_then(|os_release| etc::os_name(&os_release)),
        "kernel": kernel,
        "arch": arch,
        "catalog": serde_json::to_value(scripts::catalog(node)).unwrap_or(Value::Null),
    }))
}

pub fn history(node: &Node, args: &Args) -> Result<Answer> {
    let count = args.small_integer("lines")?.unwrap_or(50).max(1) as usize;
    if !fs::exists(&node.config.audit) {
        return Ok(Answer::of(json!([])));
    }
    let audit = fs::open_read(&node.config.audit).map_err(internal)?;
    let tail = fs::tail(&audit, count, count as u64 * AUDIT_BYTES_PER_LINE).map_err(internal)?;
    // The log keeps every argument as it came; what leaves the node is redacted.
    let entries: Vec<Value> =
        tail.lines.iter().filter_map(|line| serde_json::from_str(&node.redactor.redact(line)).ok()).collect();
    Ok(Answer::of(json!(entries)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use limen_core::config::node::NodeConfig;

    #[test]
    fn history_is_redacted() {
        let dir = format!("{}/limen-history-{}", std::env::temp_dir().display(), crate::hub::dir::random(8).unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let node = Node::new(NodeConfig { audit: format!("{dir}/audit.jsonl"), ..Default::default() });
        std::fs::write(
            &node.config.audit,
            "{\"request\":\"run\",\"args\":{\"script\":\"rotate\",\"args\":{\"token\":\"abc123\"}}}\n",
        )
        .unwrap();
        let history = history(&node, &Map::new()).unwrap().data.to_string();
        std::fs::remove_dir_all(&dir).ok();
        assert!(history.contains("rotate"), "{history}");
        assert!(!history.contains("abc123"), "{history}");
    }
}
