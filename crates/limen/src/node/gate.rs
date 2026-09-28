//! `limen gate`: the forced command (spec §3, §4). Reads one JSON request from stdin, answers it and exits. It always
//! answers JSON on stdout, even when the configuration is broken.

use super::{Answer, Node, requests as answers};
use crate::os::{fs, proc, sys};
use limen_core::config::node::NodeConfig;
use limen_core::params;
use limen_core::protocol::{ErrorCode, NodeRequest, NodeResponse, PROTOCOL_VERSIONS, Result, bad_request, error};
use limen_core::requests;
use limen_core::time::iso;
use serde_json::{Value, json};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 1024 * 1024;
const MAX_AUDIT_ARGS: usize = 4096;
const MAX_AUDIT_NAME: usize = 64;
/// How long a client may take to send its request.
const STDIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a request may take in all. A script gets its own timeout instead (see [allow]).
const REQUEST_TIME: Duration = Duration::from_secs(120);

/// This request until its audit record is written. Whoever takes it —the request when it ends, or the watchdog when
/// it doesn't in time— answers and writes the record; the other stays silent.
struct Pending {
    started: Instant,
    /// The defaults until the configuration is read.
    reporting: Reporting,
    request: Option<NodeRequest>,
}

/// Where a request's audit record goes and how large its answer may be.
struct Reporting {
    audit: String,
    audit_max_bytes: u64,
    max_response: usize,
}

impl Reporting {
    fn of(config: &NodeConfig) -> Self {
        Self {
            audit: config.audit.clone(),
            audit_max_bytes: config.audit_max_bytes,
            max_response: config.max_response_bytes,
        }
    }
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

// A panic elsewhere must not stop the gate from answering and writing its audit record: a poisoned lock is still used.
fn lock_pending() -> MutexGuard<'static, Option<Pending>> {
    PENDING.lock().expect("nothing panics holding the pending request")
}

fn lock_deadline() -> MutexGuard<'static, Option<Instant>> {
    DEADLINE.lock().expect("nothing panics holding the deadline")
}

/// Updates the request's record, unless the watchdog has taken it.
fn update_pending(update: impl FnOnce(&mut Pending)) {
    if let Some(pending) = lock_pending().as_mut() {
        update(pending);
    }
}

/// Gives the request [more] time from now: a script, its own timeout.
pub fn allow(more: Duration) {
    if let Some(deadline) = lock_deadline().as_mut() {
        *deadline = (*deadline).max(Instant::now() + more);
    }
}

/// Ends a request that outlives its deadline: what it started is stopped, the client is told, and the audit log says
/// so.
fn watchdog() {
    std::thread::spawn(|| {
        loop {
            let Some(deadline) = *lock_deadline() else { return };
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
                continue;
            }
            let Some(pending) = lock_pending().take() else { return };
            proc::stop_all();
            let spent = pending.started.elapsed().as_secs().max(1);
            let timeout = error(ErrorCode::Timeout, format!("the request did not finish in {spent}s"));
            send(pending.reporting.max_response, &NodeResponse::failure(&timeout));
            audit(&pending, timeout.code.wire(), None);
            std::process::exit(0);
        }
    });
}

pub fn run(config_path: &str) -> i32 {
    sys::chdir_root();
    sys::umask_022();
    let started = Instant::now();
    let reporting = Reporting::of(&NodeConfig::default());
    *lock_pending() = Some(Pending { started, reporting, request: None });
    *lock_deadline() = Some(started + STDIN_TIMEOUT + REQUEST_TIME);
    watchdog();
    // Held until the answer is written.
    let mut slot = None;
    let outcome = handle(config_path, &mut slot);
    let Some(pending) = lock_pending().take() else {
        // The watchdog answered and is ending the process.
        loop {
            std::thread::park();
        }
    };
    let max_response = pending.reporting.max_response;
    match outcome {
        Ok(answer) => {
            let exit = answer.data.get("exit").and_then(Value::as_i64);
            send(max_response, &NodeResponse::success(answer.data, answer.truncated));
            audit(&pending, "ok", exit);
        }
        Err(failure) => {
            send(max_response, &NodeResponse::failure(&failure));
            audit(&pending, failure.code.wire(), None);
        }
    }
    drop(slot);
    0
}

/// Reads the configuration and the request, takes one of the node's places into [slot], and does what it asks. A
/// script's run is audited when it starts too: it may change the machine, and never end.
fn handle(config_path: &str, slot: &mut Option<std::fs::File>) -> Result<Answer> {
    let node = Node::load(config_path)?;
    update_pending(|pending| pending.reporting = Reporting::of(&node.config));
    let request = parse(sys::read_stdin(MAX_REQUEST, STDIN_TIMEOUT))?;
    update_pending(|pending| pending.request = Some(request.clone()));
    let definition = requests::find(&request.request)
        .ok_or_else(|| bad_request(format!("unknown request '{}'", shortened(&request.request))))?;
    let args = params::validate(&definition.params, &request.args)?;
    *slot = take_slot(node.config.concurrency)?;
    if definition.name == "run" {
        if let Some(pending) = lock_pending().as_ref() {
            audit(pending, "started", None);
        }
    }
    answers::answer(&node, definition.name, &args)
}

/// One of `limits.concurrency` places for a request, whatever hub it comes from: a lock on `/run/limen/slot-<n>`,
/// held while the request runs. Where those can't be made —limen not running as root— there is no limit.
fn take_slot(slots: usize) -> Result<Option<std::fs::File>> {
    let dir = if fs::is_directory("/run") { "/run/limen" } else { "/var/run/limen" };
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
        return Err(bad_request(r#"no request on stdin; send one JSON object, e.g. {"v":1,"request":"hello"}"#));
    }
    let request: NodeRequest = serde_json::from_str(text).map_err(|malformed| {
        bad_request(format!("malformed request: {}", malformed.to_string().lines().next().unwrap_or("")))
    })?;
    if !PROTOCOL_VERSIONS.contains(&request.version) {
        let mut unsupported = error(
            ErrorCode::UnsupportedVersion,
            format!("protocol version {} is not supported by limen on this node", request.version),
        );
        unsupported.versions = Some(PROTOCOL_VERSIONS.to_vec());
        return Err(unsupported);
    }
    Ok(request)
}

/// Writes the answer on stdout; one over [max_response] becomes an error that says so.
fn send(max_response: usize, response: &NodeResponse) {
    let mut text = serde_json::to_string(response).unwrap_or_default();
    if text.len() > max_response {
        let too_large = bad_request(format!(
            "the answer is {} bytes, over limits.max_response ({max_response}); narrow the request",
            text.len()
        ));
        text = serde_json::to_string(&NodeResponse::failure(&too_large)).unwrap_or_default();
    }
    sys::say(&text);
}

/// A request's name as the client sent it, up to a length.
fn shortened(name: &str) -> String {
    if name.chars().count() <= MAX_AUDIT_NAME {
        name.to_string()
    } else {
        format!("{}… ({} bytes)", name.chars().take(MAX_AUDIT_NAME).collect::<String>(), name.len())
    }
}

/// The node's audit log (spec §8): one JSON line per request, whatever its outcome; a script's [exit] code when it ran.
fn audit(pending: &Pending, result: &str, exit: Option<i64>) {
    let path = &pending.reporting.audit;
    let line = audit_record(pending, result, exit).to_string();
    if let Err(failure) = append_rotating(path, pending.reporting.audit_max_bytes, &line) {
        sys::log(&format!("cannot write the audit log {path}: {failure}"));
    }
}

fn audit_record(pending: &Pending, result: &str, exit: Option<i64>) -> Value {
    // A request's name and arguments, up to a size: a megabyte of them per request would rotate the log out.
    let request = pending.request.as_ref();
    let args = request.map_or_else(|| json!({}), |request| Value::Object(request.args.clone()));
    let size = args.to_string().len();
    let mut record = json!({
        "time": iso(limen_core::time::now()),
        "request": request.map(|request| shortened(&request.request)),
        "args": if size <= MAX_AUDIT_ARGS { args } else { json!({"omitted_bytes": size}) },
        "client": sys::env("SSH_CONNECTION").and_then(|connection| connection.split(' ').next().map(String::from)),
        "result": result,
        "duration_ms": pending.started.elapsed().as_millis() as u64,
    });
    if let Some(exit) = exit {
        record["exit"] = json!(exit);
    }
    record
}

/// Appends [line] to the log at [path], moving it to `<path>.1` first once it is over [max_bytes]. One old file kept,
/// no logrotate needed: OpenWrt has none, and its /var/log lives in RAM.
fn append_rotating(path: &str, max_bytes: u64, line: &str) -> std::result::Result<(), String> {
    let dir = fs::parent(path);
    fs::mkdirs(dir, 0o700)?;
    if fs::stat(path).is_some_and(|info| info.size > max_bytes) {
        std::fs::rename(path, format!("{path}.1")).ok();
    }
    fs::append_line(path, line)
}
