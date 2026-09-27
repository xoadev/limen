//! `limen gate --role <role>`: the forced command (spec §3, §4). Reads one JSON request from stdin, answers it within
//! the role and exits. A read request always gets a JSON answer on stdout, even when the configuration is broken; a
//! deploy request streams its scripts' output as text and exits with their result.

use super::{Answer, Node, deploy, read};
use crate::os::{fs, proc, sys};
use limen_core::config::node::NodeConfig;
use limen_core::params;
use limen_core::protocol::{
    ErrorCode, LimenError, NodeRequest, NodeResponse, PROTOCOL_VERSIONS, Result, bad_request, error,
};
use limen_core::requests::{self, Role};
use limen_core::time::iso;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 1024 * 1024;
const MAX_AUDIT_ARGS: usize = 4096;
const MAX_AUDIT_NAME: usize = 64;
/// How long a client may take to send its request.
const STDIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a read request may take in all. A check gets its own timeout on top (see [allow]).
const READ_DEADLINE: Duration = Duration::from_secs(120);

/// This request until its audit record is written. Whoever takes it —the request when it ends, or the watchdog when
/// it doesn't in time— answers and writes the record; the other stays silent.
struct Pending {
    role: Role,
    started: Instant,
    audit: String,
    audit_max_bytes: u64,
    max_response: usize,
    request: Option<NodeRequest>,
}

/// How a request ended, when it did.
enum Done {
    Answered(Answer),
    Deployed { succeeded: bool },
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives a read request [more] time from now: a check, its script's timeout.
pub fn allow(more: Duration) {
    if let Some(deadline) = DEADLINE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        *deadline = (*deadline).max(Instant::now() + more);
    }
}

/// Ends a read request that outlives its deadline: what it started is stopped, the client is told, and the audit log
/// says so.
fn watchdog() {
    std::thread::spawn(|| {
        loop {
            let Some(deadline) = *DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) else { return };
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
                continue;
            }
            let Some(p) = pending().take() else { return };
            proc::stop_all();
            let spent = p.started.elapsed().as_secs().max(1);
            let e = error(ErrorCode::Timeout, format!("the request did not finish in {spent}s"));
            write(p.max_response, &NodeResponse::failure(&e));
            audit(&p, e.code.wire());
            std::process::exit(0);
        }
    });
}

pub fn run(role: Role, config_path: &str) -> i32 {
    sys::chdir_root();
    let started = Instant::now();
    let defaults = NodeConfig::default();
    *pending() = Some(Pending {
        role,
        started,
        audit: defaults.audit.clone(),
        audit_max_bytes: defaults.audit_max_bytes,
        max_response: defaults.max_response_bytes,
        request: None,
    });
    if role == Role::Read {
        *DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(started + STDIN_TIMEOUT + READ_DEADLINE);
        watchdog();
    }
    // Held until the answer is written.
    let mut slot = None;
    let outcome = (|| -> Result<Done> {
        let node = Node::load(config_path)?;
        if let Some(p) = pending().as_mut() {
            p.audit = node.config.audit.clone();
            p.audit_max_bytes = node.config.audit_max_bytes;
            p.max_response = node.config.max_response_bytes;
        }
        let r = parse(sys::read_stdin(MAX_REQUEST, STDIN_TIMEOUT))?;
        if let Some(p) = pending().as_mut() {
            p.request = Some(r.clone());
        }
        let def = requests::find(&r.request)
            .ok_or_else(|| bad_request(format!("unknown request '{}'", short(&r.request))))?;
        if def.role != role {
            return Err(error(
                ErrorCode::Denied,
                format!("'{}' is not allowed for the {} role", def.name, role.wire()),
            ));
        }
        let args = params::validate(&def.params, &r.args)?;
        if role == Role::Deploy {
            return Ok(Done::Deployed { succeeded: deploy::run(&node, def.name, &args)? });
        }
        slot = take_slot(node.config.concurrency)?;
        read::answer(&node, def.name, &args).map(Done::Answered)
    })();
    let Some(p) = pending().take() else {
        // The watchdog answered and is ending the process.
        loop {
            std::thread::park();
        }
    };
    let (code, exit) = match outcome {
        Ok(Done::Answered(answer)) => {
            write(p.max_response, &NodeResponse::success(answer.data, answer.truncated));
            ("ok", 0)
        }
        Ok(Done::Deployed { succeeded: true }) => ("ok", 0),
        Ok(Done::Deployed { succeeded: false }) => ("failed", 1),
        Err(e) => (e.code.wire(), fail(role, p.max_response, &e)),
    };
    audit(&p, code);
    drop(slot);
    exit
}

/// One of `limits.concurrency` places for a read request, whatever hub it comes from: a lock on `/run/limen/slot-<n>`,
/// held while the request runs. Where those can't be made —limen not running as root— there is no limit.
fn take_slot(slots: usize) -> Result<Option<std::fs::File>> {
    let dir = if fs::stat("/run").is_some_and(|i| i.kind == fs::FileType::Directory) {
        "/run/limen"
    } else {
        "/var/run/limen"
    };
    if fs::mkdirs(dir, 0o700).is_err() {
        return Ok(None);
    }
    for n in 0..slots {
        match fs::try_lock(&format!("{dir}/slot-{n}")) {
            Ok(Some(lock)) => return Ok(Some(lock)),
            Ok(None) => {}
            Err(_) => return Ok(None),
        }
    }
    Err(error(
        ErrorCode::Unavailable,
        format!("{slots} requests are already running on this node (limits.concurrency); try again later"),
    ))
}

fn parse(bytes: std::result::Result<Vec<u8>, String>) -> Result<NodeRequest> {
    let bytes = bytes.map_err(bad_request)?;
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

fn write(limit: usize, response: &NodeResponse) {
    let mut text = serde_json::to_string(response).unwrap_or_default();
    if text.len() > limit {
        let e = bad_request(format!(
            "the answer is {} bytes, over limits.max_response ({limit}); narrow the request",
            text.len()
        ));
        text = serde_json::to_string(&NodeResponse::failure(&e)).unwrap_or_default();
    }
    sys::out(&format!("{text}\n"));
}

fn fail(role: Role, max_response: usize, e: &LimenError) -> i32 {
    if role == Role::Deploy {
        sys::err(&format!("limen: {}: {}\n", e.code.wire(), e.message));
        return 1;
    }
    write(max_response, &NodeResponse::failure(e));
    0
}

/// A request's name as the client sent it, up to a length.
fn short(name: &str) -> String {
    if name.chars().count() <= MAX_AUDIT_NAME {
        name.to_string()
    } else {
        format!("{}… ({} bytes)", name.chars().take(MAX_AUDIT_NAME).collect::<String>(), name.len())
    }
}

/// The node's audit log (spec §8): one JSON line per request, whatever its outcome.
fn audit(p: &Pending, code: &str) {
    // A request's name and arguments, up to a size: a megabyte of them per request would rotate the log out.
    let request = p.request.as_ref();
    let args = request.map(|r| Value::Object(r.args.clone())).unwrap_or(json!({}));
    let size = args.to_string().len();
    let name = request.map(|r| short(&r.request));
    let entry = json!({
        "time": iso(limen_core::time::now()),
        "role": p.role.wire(),
        "request": name,
        "args": if size <= MAX_AUDIT_ARGS { args } else { json!({"omitted_bytes": size}) },
        "client": sys::env("SSH_CONNECTION").and_then(|c| c.split(' ').next().map(String::from)),
        "user": sys::env("SUDO_USER"),
        "result": code,
        "duration_ms": p.started.elapsed().as_millis() as u64,
    });
    let path = &p.audit;
    let result = (|| {
        let dir = path.rsplit_once('/').map_or("/", |(d, _)| d);
        fs::mkdirs(dir, 0o700)?;
        // One old file kept, no logrotate needed: OpenWrt has none, and its /var/log lives in RAM.
        if fs::stat(path).is_some_and(|i| i.size > p.audit_max_bytes) {
            std::fs::rename(path, format!("{path}.1")).ok();
        }
        fs::append_line(path, &entry.to_string())
    })();
    if let Err(e) = result {
        sys::err(&format!("limen: cannot write the audit log {path}: {e}\n"));
    }
}
