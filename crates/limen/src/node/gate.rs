//! `limen gate --role <role>`: the forced command (spec §3, §4). Reads one JSON request from stdin, answers it within
//! the role and exits. A read request always gets a JSON answer on stdout, even when the configuration is broken; a
//! deploy request streams its scripts' output as text and exits with their result.

use super::{Node, deploy, read};
use crate::os::{fs, sys};
use limen_core::config::node::NodeConfig;
use limen_core::params;
use limen_core::protocol::{
    ErrorCode, LimenError, NodeRequest, NodeResponse, PROTOCOL_VERSIONS, Result, bad_request, error,
};
use limen_core::requests::{self, Role};
use limen_core::time::iso;
use serde_json::{Value, json};
use std::time::Instant;

const MAX_REQUEST: usize = 1024 * 1024;
const MAX_AUDIT_ARGS: usize = 4096;

pub fn run(role: Role, config_path: &str) -> i32 {
    sys::chdir_root();
    let started = Instant::now();
    let mut request: Option<NodeRequest> = None;
    let mut node: Option<Node> = None;
    let outcome = (|| -> Result<i32> {
        node = Some(Node::load(config_path)?);
        let node = node.as_ref().expect("just loaded");
        let r = parse(sys::read_stdin(MAX_REQUEST))?;
        request = Some(r.clone());
        let def = requests::find(&r.request).ok_or_else(|| bad_request(format!("unknown request '{}'", r.request)))?;
        if def.role != role {
            return Err(error(
                ErrorCode::Denied,
                format!("'{}' is not allowed for the {} role", def.name, role.wire()),
            ));
        }
        let args = params::validate(&def.params, &r.args)?;
        if role == Role::Deploy {
            return Ok(if deploy::run(node, def.name, &args)? { 0 } else { 1 });
        }
        let answer = read::answer(node, def.name, &args)?;
        write(Some(node), &NodeResponse::success(answer.data, answer.truncated));
        Ok(0)
    })();
    let (code, exit) = match outcome {
        Ok(0) => ("ok".to_string(), 0),
        Ok(n) => ("failed".to_string(), n),
        Err(e) => (e.code.wire().to_string(), fail(role, node.as_ref(), &e)),
    };
    // Where the record goes: the configured log, or the default one when the configuration can't be read.
    let audit_to = node.unwrap_or_else(|| Node::new(NodeConfig::default()));
    audit(&audit_to, role, request.as_ref(), &code, started.elapsed().as_millis() as u64);
    exit
}

fn parse(bytes: Option<Vec<u8>>) -> Result<NodeRequest> {
    let bytes = bytes.ok_or_else(|| bad_request(format!("request larger than {MAX_REQUEST} bytes")))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        return Err(bad_request(r#"no request on stdin; send one JSON object, e.g. {"v":1,"request":"status"}"#));
    }
    let request: NodeRequest = serde_json::from_str(text)
        .map_err(|e| bad_request(format!("malformed request: {}", e.to_string().lines().next().unwrap_or(""))))?;
    if !PROTOCOL_VERSIONS.contains(&request.v) {
        let mut e = error(
            ErrorCode::UnsupportedVersion,
            format!("protocol version {} is not supported by limen on this node", request.v),
        );
        e.versions = Some(PROTOCOL_VERSIONS.to_vec());
        return Err(e);
    }
    Ok(request)
}

fn write(node: Option<&Node>, response: &NodeResponse) {
    let mut text = serde_json::to_string(response).unwrap_or_default();
    let limit = node.map_or(NodeConfig::default().max_response_bytes, |n| n.config.max_response_bytes);
    if text.len() > limit {
        let e = bad_request(format!(
            "the answer is {} bytes, over limits.max_response ({limit}); narrow the request",
            text.len()
        ));
        text = serde_json::to_string(&NodeResponse::failure(&e)).unwrap_or_default();
    }
    sys::out(&format!("{text}\n"));
}

fn fail(role: Role, node: Option<&Node>, e: &LimenError) -> i32 {
    if role == Role::Deploy {
        sys::err(&format!("limen: {}: {}\n", e.code.wire(), e.message));
        return 1;
    }
    write(node, &NodeResponse::failure(e));
    0
}

/// The node's audit log (spec §8): one JSON line per request, whatever its outcome.
fn audit(node: &Node, role: Role, request: Option<&NodeRequest>, code: &str, duration_ms: u64) {
    // A request's arguments, up to a size: a megabyte of them per request would rotate the log out.
    let args = request.map(|r| Value::Object(r.args.clone())).unwrap_or(json!({}));
    let size = args.to_string().len();
    let entry = json!({
        "time": iso(node.now()),
        "role": role.wire(),
        "request": request.map(|r| r.request.as_str()),
        "args": if size <= MAX_AUDIT_ARGS { args } else { json!({"omitted_bytes": size}) },
        "client": sys::env("SSH_CONNECTION").and_then(|c| c.split(' ').next().map(String::from)),
        "user": sys::env("SUDO_USER"),
        "result": code,
        "duration_ms": duration_ms,
    });
    let path = &node.config.audit;
    let result = (|| {
        let dir = path.rsplit_once('/').map_or("/", |(d, _)| d);
        fs::mkdirs(dir, 0o700)?;
        // One old file kept, no logrotate needed: OpenWrt has none, and its /var/log lives in RAM.
        if fs::stat(path).is_some_and(|i| i.size > node.config.audit_max_bytes) {
            std::fs::rename(path, format!("{path}.1")).ok();
        }
        fs::append_line(path, &entry.to_string())
    })();
    if let Err(e) = result {
        sys::err(&format!("limen: cannot write the audit log {path}: {e}\n"));
    }
}
