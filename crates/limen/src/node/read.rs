//! The read requests (spec §5). Each takes validated arguments and answers JSON.

use super::scripts as node_scripts;
use super::system::{self, Init, LogFilter};
use super::{Answer, Node, internal};
use crate::os::{fs, proc, sys};
use limen_core::durations;
use limen_core::params::{ArgsExt, full_match};
use limen_core::path_policy::{self, Decision};
use limen_core::protocol::{ErrorCode, LimenError, PROTOCOL_VERSIONS, Result, bad_request, error};
use limen_core::requests::{CONTAINER, UNIT};
use limen_core::scripts::ScriptKind;
use limen_core::system::parsers;
use limen_core::time::{iso, parse_iso};
use limen_core::version::VERSION;
use serde_json::{Map, Value, json};
use std::time::Duration;

type Args = Map<String, Value>;

const PRIVATE_KEY: &[u8] = b"PRIVATE KEY-----";
const MAX_SCAN_BYTES: u64 = 16 << 20;
const KEY_FILE_MAX_BYTES: u64 = 1 << 20;
const MINUTE: Duration = Duration::from_secs(60);

pub fn hello(node: &Node) -> Answer {
    let (kernel, arch) = sys::uname();
    let mut o = json!({
        "version": VERSION,
        "protocols": PROTOCOL_VERSIONS,
        "hostname": sys::hostname(),
        "os": fs::read_following("/etc/os-release").and_then(|t| parsers::os_name(&t)),
        "kernel": kernel,
        "arch": arch,
        "init": system::init().wire(),
    });
    if let Some(board) = system::board() {
        o["board"] = json!(board);
    }
    o["docker"] = json!(proc::which("docker").is_some());
    o["repo"] = json!(node.config.repo.is_some());
    o["catalog"] = serde_json::to_value(node_scripts::catalog(node)).unwrap_or(Value::Null);
    Answer::of(o)
}

pub fn status(node: &Node) -> Answer {
    let mut errors = Map::new();
    let mut part = |name: &str, r: Result<Value>| match r {
        Ok(v) => v,
        Err(e) => {
            errors.insert(name.into(), json!(e.message));
            Value::Null
        }
    };
    let uptime = part(
        "uptime",
        fs::read_text("/proc/uptime")
            .and_then(|t| t.split(' ').next().and_then(|u| u.parse::<f64>().ok()))
            .map(|u| json!(u as i64))
            .ok_or_else(|| internal("cannot read /proc/uptime")),
    );
    let load = part(
        "load",
        fs::read_text("/proc/loadavg")
            .map(|t| json!(t.split(' ').take(3).filter_map(|l| l.parse::<f64>().ok()).collect::<Vec<_>>()))
            .ok_or_else(|| internal("cannot read /proc/loadavg")),
    );
    let memory = part(
        "memory",
        fs::read_text("/proc/meminfo")
            .map(|t| parsers::memory(&t))
            .ok_or_else(|| internal("cannot read /proc/meminfo")),
    );
    let disks = part("disks", system::disks());
    let failed = part("failed_services", system::failed_services(node).map(|f| json!(f)));
    let containers = if proc::which("docker").is_none() {
        Value::Null
    } else {
        part(
            "containers",
            inspect_all(node).map(|all| {
                let attention: Vec<Value> = all
                    .iter()
                    .map(parsers::container_summary)
                    .filter(|c| c["state"] != "running" || c["health"] == "unhealthy")
                    .collect();
                json!(attention)
            }),
        )
    };
    let mut o = json!({
        "hostname": sys::hostname(),
        "uptime_seconds": uptime,
        "load": load,
        "memory": memory,
        "disks": disks,
        "failed_services": failed,
        "containers_attention": containers,
        "reboot_required": fs::exists("/run/reboot-required"),
    });
    if !errors.is_empty() {
        o["errors"] = Value::Object(errors);
    }
    Answer::of(o)
}

pub fn services(node: &Node, args: &Args) -> Result<Answer> {
    match system::init() {
        Init::Procd => return system::procd_units(node, args.string("state"), args.string("pattern")).map(Answer::of),
        Init::None => return Err(error(ErrorCode::Unavailable, "no systemd or procd on this node")),
        Init::Systemd => {}
    }
    let mut argv = vec!["systemctl", "list-units", "--no-legend", "--plain", "--no-pager"];
    let type_arg;
    if let Some(t) = args.string("type").filter(|t| *t != "all") {
        type_arg = format!("--type={t}");
        argv.push(&type_arg);
    }
    let state_arg;
    match args.string("state") {
        Some("all") => argv.push("--all"),
        Some("inactive") => argv.extend(["--all", "--state=inactive"]),
        None => {}
        Some(s) => {
            state_arg = format!("--state={s}");
            argv.push(&state_arg);
        }
    }
    if let Some(p) = args.string("pattern") {
        argv.extend(["--", p]);
    }
    Ok(Answer::of(parsers::units(&node.exec_ok(&argv)?)))
}

pub fn service(node: &Node, args: &Args) -> Result<Answer> {
    let raw = args.string("name").unwrap_or_default();
    let lines = args.int("lines")?.unwrap_or(20).max(0) as usize;
    match system::init() {
        Init::Procd => return system::procd_service(node, raw.trim_end_matches(".service"), lines).map(Answer::of),
        Init::None => return Err(error(ErrorCode::Unavailable, "no systemd or procd on this node")),
        Init::Systemd => {}
    }
    let name = if raw.contains('.') { raw.to_string() } else { format!("{raw}.service") };
    let props = parsers::key_values(&node.exec_ok(&[
        "systemctl",
        "show",
        "--no-pager",
        "--property=Id,Description,LoadState,ActiveState,SubState,Result,UnitFileState,FragmentPath,MainPID,\
         ExecMainStatus,NRestarts,MemoryCurrent,ActiveEnterTimestamp,StateChangeTimestamp,Type,Restart",
        "--",
        &name,
    ])?);
    if props.get("LoadState").map(String::as_str) == Some("not-found") {
        return Err(error(ErrorCode::NotFound, format!("no unit named {name}")));
    }
    let journal = if lines > 0 {
        let n = lines.to_string();
        parsers::journal(
            &node.exec_ok(&["journalctl", "-u", &name, "-n", &n, "-o", "json", "--no-pager", "-q"])?,
            &node.redactor,
        )
    } else {
        vec![]
    };
    let mut o = parsers::unit(&props);
    o["journal"] = json!(journal);
    Ok(Answer::of(o))
}

pub fn containers(node: &Node, args: &Args) -> Result<Answer> {
    let all = args.bool("all").unwrap_or(true);
    let list: Vec<Value> =
        inspect_all(node)?.iter().map(parsers::container_summary).filter(|c| all || c["state"] == "running").collect();
    Ok(Answer::of(json!(list)))
}

pub fn container(node: &Node, args: &Args) -> Result<Answer> {
    let name = args.string("name").unwrap_or_default();
    let r = node.exec(&["docker", "inspect", "--type", "container", "--", name], MINUTE)?;
    if r.exit_code != 0 {
        let message = r.err();
        if message.contains("No such") {
            return Err(error(ErrorCode::NotFound, format!("no container named {name}")));
        }
        return Err(docker_error(message.trim()));
    }
    let list: Vec<Map<String, Value>> = serde_json::from_str(&r.out()).unwrap_or_default();
    let o = list.into_iter().next().ok_or_else(|| error(ErrorCode::NotFound, format!("no container named {name}")))?;
    let digests: Vec<String> = match o.get("Image").and_then(Value::as_str) {
        Some(id) => node
            .exec(&["docker", "image", "inspect", "--format", "{{json .RepoDigests}}", "--", id], MINUTE)
            .ok()
            .filter(|r| r.exit_code == 0)
            .and_then(|r| serde_json::from_str::<Vec<String>>(r.out().trim()).ok())
            .unwrap_or_default(),
        None => vec![],
    };
    Ok(Answer::of(parsers::container_detail(&o, &digests, &node.redactor)))
}

fn inspect_all(node: &Node) -> Result<Vec<Map<String, Value>>> {
    let ids = node.exec(&["docker", "ps", "-aq", "--no-trunc"], MINUTE)?;
    if ids.exit_code != 0 {
        return Err(docker_error(ids.err().trim()));
    }
    let out = ids.out();
    let list: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if list.is_empty() {
        return Ok(vec![]);
    }
    let mut argv = vec!["docker", "inspect", "--type", "container"];
    argv.extend(&list);
    let r = node.exec(&argv, MINUTE)?;
    if r.exit_code != 0 && r.out().trim().is_empty() {
        return Err(docker_error(r.err().trim()));
    }
    Ok(serde_json::from_str(&r.out()).unwrap_or_default())
}

fn docker_error(message: &str) -> LimenError {
    let last = message.lines().last().unwrap_or("failed");
    if message.contains("Cannot connect") || message.contains("permission denied") {
        error(ErrorCode::Unavailable, format!("docker: {last}"))
    } else {
        internal(format!("docker: {last}"))
    }
}

pub fn logs(node: &Node, args: &Args) -> Result<Answer> {
    let source = args.string("source").unwrap_or_default();
    let name = args.string("name");
    let requested = args.int("lines")?.unwrap_or(200).max(1) as usize;
    let lines = requested.min(node.config.max_lines);
    let clamped = lines < requested;
    let grep = args.string("grep");
    let since = args.string("since").map(|t| instant(node, t)).transpose()?;
    let until = args.string("until").map(|t| instant(node, t)).transpose()?;
    let answer = match source {
        "unit" | "journal" => {
            if source == "journal" && name.is_some() {
                return Err(bad_request("source journal takes no name"));
            }
            if source == "unit" && name.is_none() {
                return Err(bad_request("source unit needs a name"));
            }
            match system::init() {
                Init::None => return Err(error(ErrorCode::Unavailable, "no journal or logread on this node")),
                Init::Procd => {
                    let filter = LogFilter {
                        source: name.map(|n| n.trim_end_matches(".service")),
                        priority: args.string("priority"),
                        since,
                        until,
                        grep,
                    };
                    Answer::of(json!(system::logread(node, lines, &filter)?))
                }
                Init::Systemd => journal(node, source, name, lines, since, until, args.string("priority"), grep)?,
            }
        }
        "container" => container_logs(
            node,
            name.ok_or_else(|| bad_request("source container needs a name"))?,
            lines,
            grep,
            since,
            until,
        )?,
        "file" => {
            if since.is_some() || until.is_some() {
                return Err(bad_request("since and until do not apply to files"));
            }
            file_logs(node, name.ok_or_else(|| bad_request("source file needs a name"))?, lines, grep)?
        }
        other => return Err(bad_request(format!("unknown source {other}"))),
    };
    Ok(if clamped { Answer::cut(answer.data, true) } else { answer })
}

#[allow(clippy::too_many_arguments)]
fn journal(
    node: &Node,
    source: &str,
    name: Option<&str>,
    lines: usize,
    since: Option<i64>,
    until: Option<i64>,
    priority: Option<&str>,
    grep: Option<&str>,
) -> Result<Answer> {
    let mut argv: Vec<String> = ["journalctl", "-o", "json", "--no-pager", "-q", "-n"].map(String::from).to_vec();
    argv.push(lines.to_string());
    if source == "unit" {
        let unit = name.unwrap_or_default();
        if !full_match(UNIT).is_ok_and(|r| r.is_match(unit)) {
            return Err(bad_request(format!("'{unit}' is not a unit name")));
        }
        argv.extend(["-u".into(), unit.into()]);
    }
    if let Some(s) = since {
        argv.push(format!("--since={}", journal_time(s)));
    }
    if let Some(u) = until {
        argv.push(format!("--until={}", journal_time(u)));
    }
    if let Some(p) = priority {
        argv.push(format!("--priority={p}"));
    }
    if let Some(g) = grep {
        argv.extend([format!("--grep={}", pcre_literal(g)), "--case-sensitive=false".into()]);
    }
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let r = node.exec(&argv, MINUTE)?;
    // journalctl --grep exits 1 when nothing matches: that is an empty answer, not an error.
    let (out, err) = (r.out(), r.err());
    if r.exit_code != 0 && out.trim().is_empty() && !err.trim().is_empty() {
        return Err(internal(format!("journalctl: {}", err.trim().lines().last().unwrap_or(""))));
    }
    Ok(Answer::cut(json!(parsers::journal(&out, &node.redactor)), r.truncated))
}

fn container_logs(
    node: &Node,
    name: &str,
    lines: usize,
    grep: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Answer> {
    if !full_match(CONTAINER).is_ok_and(|r| r.is_match(name)) {
        return Err(bad_request(format!("'{name}' is not a container name")));
    }
    let tail = if grep.is_none() { lines } else { node.config.scan_lines }.to_string();
    let mut argv: Vec<String> = ["docker", "logs", "--timestamps", "--tail", &tail].map(String::from).to_vec();
    if let Some(s) = since {
        argv.push(format!("--since={}", iso(s)));
    }
    if let Some(u) = until {
        argv.push(format!("--until={}", iso(u)));
    }
    argv.extend(["--".into(), name.into()]);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let r = node.exec(&argv, MINUTE)?;
    if r.exit_code != 0 {
        let err = r.err();
        if err.contains("No such container") {
            return Err(error(ErrorCode::NotFound, format!("no container named {name}")));
        }
        return Err(docker_error(err.trim()));
    }
    let mut merged = parsers::docker_log_lines(&r.out(), "stdout");
    merged.extend(parsers::docker_log_lines(&r.err(), "stderr"));
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    let grep = grep.map(str::to_lowercase);
    let matched: Vec<_> =
        merged.into_iter().filter(|(_, _, m)| grep.as_ref().is_none_or(|g| m.to_lowercase().contains(g))).collect();
    let from = matched.len().saturating_sub(lines);
    let rows: Vec<Value> = matched[from..]
        .iter()
        .map(
            |(time, stream, message)| json!({"time": time, "stream": stream, "message": node.redactor.redact(message)}),
        )
        .collect();
    Ok(Answer::cut(json!(rows), r.truncated))
}

fn file_logs(node: &Node, name: &str, lines: usize, grep: Option<&str>) -> Result<Answer> {
    let path = allowed_file(node, name)?;
    let scan = if grep.is_none() { lines } else { node.config.scan_lines };
    let slice = fs::tail(&path, scan, (scan as u64 * 1024).min(MAX_SCAN_BYTES), true).map_err(internal)?;
    if slice.binary {
        return Err(bad_request(format!("{path} is binary")));
    }
    // Redacted before it is cut into lines: a private key the window holds whole spans several of them.
    let redacted = node.redactor.redact(&slice.lines.join("\n"));
    let grep = grep.map(str::to_lowercase);
    let matched: Vec<&str> =
        redacted.split('\n').filter(|l| grep.as_ref().is_none_or(|g| l.to_lowercase().contains(g))).collect();
    let from = matched.len().saturating_sub(lines);
    Ok(Answer::of(json!({"path": path, "lines": matched[from..]})))
}

pub fn read_file(node: &Node, args: &Args) -> Result<Answer> {
    let path = allowed_file(node, args.string("path").unwrap_or_default())?;
    let from = args.int("from")?.unwrap_or(1).max(1) as usize;
    let count = args.int("lines")?.unwrap_or(500).max(1) as usize;
    let size = fs::stat(&path).map_or(0, |i| i.size);
    let slice = fs::read_lines(&path, from, count, node.config.max_file_bytes, true).map_err(internal)?;
    if slice.binary {
        return Ok(Answer::of(json!({"path": path, "size_bytes": size, "binary": true})));
    }
    let cut_by_bytes = !slice.eof && slice.lines.len() < count;
    Ok(Answer::cut(
        json!({
            "path": path,
            "size_bytes": size,
            "from": from,
            "to": from + slice.lines.len() - 1,
            "eof": slice.eof,
            "content": node.redactor.redact(&slice.lines.join("\n")),
        }),
        cut_by_bytes,
    ))
}

pub fn list_dir(node: &Node, args: &Args) -> Result<Answer> {
    let listable = |p: &str| node.policy.allowed(p) || node.policy.leads_to(p);
    let dir = resolve(node, args.string("path").unwrap_or_default(), &listable)?;
    // The policy before the type: what a denied path is, like whether it exists, is not the client's to learn.
    if !listable(&dir) {
        return Err(error(ErrorCode::Denied, denial(node, &dir)));
    }
    if fs::stat(&dir).map(|i| i.kind) != Some(fs::FileType::Directory) {
        return Err(bad_request(format!("{dir} is not a directory")));
    }
    const MAX: usize = 1000;
    let mut entries = Vec::new();
    for name in fs::list(&dir).map_err(internal)? {
        let full = if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") };
        let Some(entry) = fs::lstat(&full) else { continue };
        let target = if entry.kind == fs::FileType::Link { fs::real_path(&full) } else { Some(full.clone()) };
        let visible = target.as_deref().is_some_and(|t| {
            node.policy.allowed(t)
                || (fs::stat(t).map(|i| i.kind) == Some(fs::FileType::Directory) && node.policy.leads_to(t))
        });
        if !visible {
            continue;
        }
        let mut o = json!({
            "name": name,
            "type": entry.kind.wire(),
            "size_bytes": entry.size,
            "mode": format!("{:04o}", entry.mode & 0o7777),
            "owner": fs::user_name(entry.uid).unwrap_or_else(|| entry.uid.to_string()),
            "group": fs::group_name(entry.gid).unwrap_or_else(|| entry.gid.to_string()),
            "modified": iso(entry.modified),
        });
        if entry.kind == fs::FileType::Link {
            o["target"] = json!(target);
        }
        entries.push(o);
        if entries.len() > MAX {
            break;
        }
    }
    let truncated = entries.len() > MAX;
    entries.truncate(MAX);
    Ok(Answer::cut(json!({"path": dir, "entries": entries}), truncated))
}

pub fn processes(node: &Node, args: &Args) -> Result<Answer> {
    let limit = args.int("limit")?.unwrap_or(20).max(1) as usize;
    Ok(Answer::of(system::processes(node, args.string("sort") == Some("memory"), limit)))
}

pub fn ports() -> Answer {
    Answer::of(system::ports())
}

pub fn history(node: &Node, args: &Args) -> Result<Answer> {
    let count = args.int("lines")?.unwrap_or(50).max(1) as usize;
    if !fs::exists(&node.config.audit) {
        return Ok(Answer::of(json!([])));
    }
    let slice = fs::tail(&node.config.audit, count, count as u64 * 8192, false).map_err(internal)?;
    // The log keeps every argument as it came, deploy ones included; what leaves the node is redacted.
    let entries: Vec<Value> =
        slice.lines.iter().filter_map(|l| serde_json::from_str(&node.redactor.redact(l)).ok()).collect();
    Ok(Answer::of(json!(entries)))
}

pub fn check(node: &Node, args: &Args) -> Result<Answer> {
    let (entry, spec) = node_scripts::find(node, ScriptKind::Check, args.string("name").unwrap_or_default())?;
    let env = node_scripts::environment(&spec, &args.obj("args"))?;
    let r = node_scripts::run(&entry, &spec, env, 64 * 1024, None)?;
    if r.timed_out {
        return Err(error(
            ErrorCode::Timeout,
            format!("check {} did not finish in {}s", spec.name, spec.timeout_seconds),
        ));
    }
    let status = match r.exit_code {
        0 => "ok",
        1 => "warn",
        2 => "fail",
        _ => "unknown",
    };
    let output = node.redactor.redact(&r.out());
    let output = output.trim_end();
    let (summary, detail) = output.split_once('\n').unwrap_or((output, ""));
    let mut o = json!({"check": spec.name, "status": status, "exit_code": r.exit_code});
    if let Some(s) = r.signal {
        o["signal"] = json!(s);
    }
    o["summary"] = json!(summary);
    o["detail"] = json!(detail);
    let err = r.err();
    if !err.trim().is_empty() {
        let redacted = node.redactor.redact(&err);
        let tail: String = redacted.trim_end().chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
        o["stderr"] = json!(tail);
    }
    Ok(Answer::cut(o, r.truncated))
}

/// [requested] resolved and checked against the policy: the path that is safe to open.
pub fn allowed_file(node: &Node, requested: &str) -> Result<String> {
    let resolved = resolve(node, requested, &|p| node.policy.allowed(p))?;
    if let Decision::Denied(reason) = node.policy.check(&resolved) {
        return Err(error(ErrorCode::Denied, reason));
    }
    let info = fs::stat(&resolved).ok_or_else(|| error(ErrorCode::NotFound, format!("{resolved} does not exist")))?;
    if info.kind != fs::FileType::File {
        return Err(bad_request(format!("{resolved} is not a regular file")));
    }
    // A window of lines can fall between a key's markers, where redaction can't see it. Keys live in small files, so
    // those are searched whole; in a large one, a log, the window is redacted as a whole instead.
    if info.size <= KEY_FILE_MAX_BYTES && fs::contains(&resolved, PRIVATE_KEY, KEY_FILE_MAX_BYTES) {
        return Err(error(ErrorCode::Denied, format!("{resolved} holds a private key")));
    }
    Ok(resolved)
}

/// [requested] with symlinks and `..` resolved. A path that doesn't exist is judged by what it would be: one the
/// policy would refuse is `denied` whether or not it exists, so the answer never tells a denied path's existence.
fn resolve(node: &Node, requested: &str, visible: &dyn Fn(&str) -> bool) -> Result<String> {
    if !requested.starts_with('/') {
        return Err(bad_request(format!("{requested} is not an absolute path")));
    }
    if let Some(real) = fs::real_path(requested) {
        return Ok(real);
    }
    let judged = resolve_existing(&path_policy::normalize(requested));
    if !visible(&judged) {
        return Err(error(ErrorCode::Denied, denial(node, &judged)));
    }
    Err(error(ErrorCode::NotFound, format!("{judged} does not exist")))
}

/// [path] with its longest existing prefix resolved, so a link to a denied directory is judged where it leads.
fn resolve_existing(path: &str) -> String {
    let mut head = path.to_string();
    let mut missing: Vec<String> = Vec::new();
    while head != "/" {
        if let Some(real) = fs::real_path(&head) {
            missing.reverse();
            return path_policy::normalize(&format!("{real}/{}", missing.join("/")));
        }
        let (parent, last) = head.rsplit_once('/').unwrap_or(("", &head));
        missing.push(last.to_string());
        head = if parent.is_empty() { "/".into() } else { parent.into() };
    }
    path.to_string()
}

fn denial(node: &Node, path: &str) -> String {
    match node.policy.check(path) {
        Decision::Denied(reason) => reason,
        Decision::Allowed => format!("{path} is not readable"),
    }
}

/// `30m` (that long ago) or `2026-09-26T08:00Z`.
fn instant(node: &Node, text: &str) -> Result<i64> {
    if let Some(d) = durations::parse(text) {
        return Ok(node.now() - d.as_secs() as i64);
    }
    parse_iso(text).ok_or_else(|| bad_request(format!("'{text}' is not a time")))
}

fn journal_time(epoch: i64) -> String {
    format!("{} UTC", iso(epoch).replace('T', " ").trim_end_matches('Z'))
}

/// [text] as a PCRE pattern that matches only itself: every non-alphanumeric character escaped.
pub fn pcre_literal(text: &str) -> String {
    text.chars().map(|c| if c.is_alphanumeric() || c == ' ' { c.to_string() } else { format!("\\{c}") }).collect()
}

pub fn answer(node: &Node, name: &str, args: &Args) -> Result<Answer> {
    match name {
        "hello" => Ok(hello(node)),
        "status" => Ok(status(node)),
        "services" => services(node, args),
        "service" => service(node, args),
        "containers" => containers(node, args),
        "container" => container(node, args),
        "logs" => logs(node, args),
        "read_file" => read_file(node, args),
        "list_dir" => list_dir(node, args),
        "processes" => processes(node, args),
        "ports" => Ok(ports()),
        "history" => history(node, args),
        "state" => Ok(super::state::answer(node)),
        "check" => check(node, args),
        other => Err(bad_request(format!("unknown request '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use limen_core::config::node::NodeConfig;
    use std::os::unix::fs::symlink;

    /// A tree to read, removed at the end: allowed files, a denied directory, links out of the allowlist.
    struct Tree {
        dir: String,
        node: Node,
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn tree() -> Tree {
        let dir = format!("{}/limen-read-{}", std::env::temp_dir().display(), crate::hub::dir::random(8).unwrap());
        for d in ["etc/private", "outside"] {
            std::fs::create_dir_all(format!("{dir}/{d}")).unwrap();
        }
        let write = |p: &str, text: &str| std::fs::write(format!("{dir}/{p}"), text).unwrap();
        write("etc/app.conf", "name = app\npassword = \"two words\"\n");
        write("etc/private/real", "x\n");
        write("outside/real", "x\n");
        write(
            "etc/deploy.pem",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
        );
        symlink(format!("{dir}/outside/real"), format!("{dir}/etc/link")).unwrap();
        symlink(format!("{dir}/etc/private"), format!("{dir}/etc/privlink")).unwrap();
        let config = NodeConfig {
            allow: vec![format!("{dir}/etc/**")],
            deny: vec![format!("{dir}/etc/private/**")],
            audit: format!("{dir}/audit.jsonl"),
            ..Default::default()
        };
        Tree { node: Node::new(config), dir }
    }

    fn path(p: &str) -> Args {
        json_object_of(json!({"path": p}))
    }

    fn json_object_of(v: Value) -> Args {
        v.as_object().cloned().unwrap()
    }

    fn refused(r: Result<Answer>) -> ErrorCode {
        r.err().expect("refused").code
    }

    #[test]
    fn an_allowed_file_is_read_and_redacted() {
        let t = tree();
        let answer = read_file(&t.node, &path(&format!("{}/etc/app.conf", t.dir))).unwrap();
        assert_eq!(answer.data["content"], "name = app\npassword = \"[redacted]\"");
        assert_eq!(answer.data["to"], 2);
    }

    #[test]
    fn a_denied_path_says_nothing_of_its_existence() {
        let t = tree();
        for denied in [
            "private/real",
            "private/missing",
            "../outside/real",
            "../outside/missing",
            "link",
            "privlink/real",
            "privlink/missing",
        ] {
            let p = format!("{}/etc/{denied}", t.dir);
            assert_eq!(refused(read_file(&t.node, &path(&p))), ErrorCode::Denied, "read_file {denied}");
            assert_eq!(refused(list_dir(&t.node, &path(&p))), ErrorCode::Denied, "list_dir {denied}");
        }
        assert_eq!(refused(read_file(&t.node, &path(&format!("{}/etc/missing", t.dir)))), ErrorCode::NotFound);
        assert_eq!(refused(list_dir(&t.node, &path(&format!("{}/etc/app.conf", t.dir)))), ErrorCode::BadRequest);
    }

    #[test]
    fn listing_hides_what_cannot_be_read() {
        let t = tree();
        let answer = list_dir(&t.node, &path(&format!("{}/etc", t.dir))).unwrap();
        let names: Vec<&str> =
            answer.data["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"app.conf"), "{names:?}");
        assert!(!names.contains(&"private") && !names.contains(&"link"), "{names:?}");
    }

    #[test]
    fn a_file_holding_a_private_key_is_not_read_at_all() {
        // A window of lines between the markers would carry the key's body past redaction.
        let t = tree();
        let window = json_object_of(json!({"path": format!("{}/etc/deploy.pem", t.dir), "from": 2, "lines": 2}));
        assert_eq!(refused(read_file(&t.node, &window)), ErrorCode::Denied);
    }

    #[test]
    fn a_key_in_a_big_log_is_masked_when_the_window_holds_it() {
        // Over the size searched whole for keys: the window is redacted as one text, markers and body together.
        let t = tree();
        let filler = format!("{}\n", "x".repeat(99)).repeat(12_000);
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        std::fs::write(format!("{}/etc/big.log", t.dir), format!("{filler}{key}done\n")).unwrap();
        let args = json_object_of(json!({"source": "file", "name": format!("{}/etc/big.log", t.dir), "lines": 10}));
        let lines = logs(&t.node, &args).unwrap().data.to_string();
        assert!(lines.contains("done"), "{lines}");
        assert!(!lines.contains("b3BlbnNzaA"), "{lines}");
    }

    #[test]
    fn history_is_redacted() {
        let t = tree();
        std::fs::write(
            &t.node.config.audit,
            "{\"request\":\"action\",\"args\":{\"name\":\"rotate\",\"args\":{\"token\":\"abc123\"}}}\n",
        )
        .unwrap();
        let history = history(&t.node, &Map::new()).unwrap().data.to_string();
        assert!(history.contains("rotate"), "{history}");
        assert!(!history.contains("abc123"), "{history}");
    }

    #[test]
    fn files_that_are_not_scripts_are_ignored_not_problems() {
        // A README, a .gitkeep to keep the folder in git: neither may stop `apply` or show up as a broken script.
        let t = tree();
        let dir = format!("{}/setup", t.dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["README.md", ".gitkeep"] {
            std::fs::write(format!("{dir}/{f}"), "x\n").unwrap();
        }
        let node = Node::new(NodeConfig {
            explicit_checks: Some(dir.clone()),
            explicit_setup: Some(dir),
            ..Default::default()
        });
        let entries = node_scripts::discover(&node, ScriptKind::Setup);
        assert_eq!(entries.iter().map(|e| e.file.as_str()).collect::<Vec<_>>(), ["README.md"]);
        assert!(entries[0].ignored);
        assert!(node_scripts::catalog(&node).problems.is_empty());
    }

    #[test]
    fn a_literal_for_journalctl_grep() {
        assert_eq!(pcre_literal("a.b (c)"), "a\\.b \\(c\\)");
    }
}
