//! The read requests (spec §5). Each takes validated arguments and answers JSON.

use super::scripts as node_scripts;
use super::system::{self, IdNames, Init, LogFilter};
use super::{Answer, MINUTE, Node, internal};
use crate::os::{fs, proc, sys};
use limen_core::durations;
use limen_core::params::{ArgsExt, full_match};
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
/// What `logs` with `source = file` reads per line it answers, up to [MAX_SCAN_BYTES].
const LOG_BYTES_PER_LINE: u64 = 1024;
/// How far into a file `read_file` goes to reach its first line: further on, `logs` with `source = file` reads the end.
const MAX_SKIP_BYTES: u64 = 64 << 20;
const KEY_FILE_MAX_BYTES: u64 = 1 << 20;
/// Links followed in one path, as the kernel's own limit.
const MAX_LINKS: usize = 40;
const MAX_LISTED_ENTRIES: usize = 1000;
/// What `history` reads per audit line it answers.
const AUDIT_BYTES_PER_LINE: u64 = 8192;
/// A check's output is capped (spec §6).
const MAX_CHECK_STDERR_CHARS: usize = 4000;

pub fn hello(node: &Node) -> Answer {
    let (kernel, arch) = sys::uname();
    let mut hello = json!({
        "version": VERSION,
        "protocols": PROTOCOL_VERSIONS,
        "hostname": sys::hostname(),
        "os": fs::read_following("/etc/os-release").and_then(|os_release| parsers::os_name(&os_release)),
        "kernel": kernel,
        "arch": arch,
        "init": system::init().wire(),
    });
    if let Some(board) = system::board() {
        hello["board"] = json!(node.redactor.redact(&board));
    }
    hello["docker"] = json!(proc::which("docker").is_some());
    hello["repo"] = json!(node.config.repo.is_some());
    hello["catalog"] = serde_json::to_value(node_scripts::catalog(node)).unwrap_or(Value::Null);
    Answer::of(hello)
}

pub fn status(node: &Node) -> Answer {
    let (failed, containers) = ask_systemd_and_docker(node);
    // A part that can't be read is null, and why goes under `errors`: the rest of the status still answers.
    let mut errors = Map::new();
    let mut part = |name: &str, result: Result<Value>| match result {
        Ok(value) => value,
        Err(failure) => {
            errors.insert(name.into(), json!(node.redactor.redact(&failure.message)));
            Value::Null
        }
    };
    let uptime = part("uptime", read_uptime());
    let load = part("load", read_load());
    let memory = part("memory", read_memory());
    let disks = part("disks", system::disks());
    let failed = part("failed_services", failed);
    let containers = containers.map_or(Value::Null, |containers| part("containers", containers));
    let mut status = json!({
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
        status["errors"] = Value::Object(errors);
    }
    Answer::of(status)
}

/// The failed services, and the containers that need attention when there is Docker. Asked at once: each asks
/// another program, and takes tens of milliseconds.
fn ask_systemd_and_docker(node: &Node) -> (Result<Value>, Option<Result<Value>>) {
    std::thread::scope(|scope| {
        let failed = scope.spawn(|| system::failed_services(node).map(|names| json!(names)));
        let containers = scope.spawn(|| proc::which("docker").map(|_| containers_needing_attention(node)));
        (failed.join().expect("no panic"), containers.join().expect("no panic"))
    })
}

/// The containers that are not running, or run unhealthy.
fn containers_needing_attention(node: &Node) -> Result<Value> {
    let attention: Vec<Value> = inspect_all(node)?
        .iter()
        .map(parsers::container_summary)
        .filter(|summary| summary["state"] != "running" || summary["health"] == "unhealthy")
        .collect();
    Ok(json!(attention))
}

fn read_uptime() -> Result<Value> {
    system::uptime_seconds().map(|seconds| json!(seconds as i64)).ok_or_else(|| internal("cannot read /proc/uptime"))
}

fn read_load() -> Result<Value> {
    fs::read_text("/proc/loadavg")
        .map(|loadavg| {
            json!(loadavg.split(' ').take(3).filter_map(|figure| figure.parse::<f64>().ok()).collect::<Vec<_>>())
        })
        .ok_or_else(|| internal("cannot read /proc/loadavg"))
}

fn read_memory() -> Result<Value> {
    fs::read_text("/proc/meminfo")
        .map(|meminfo| parsers::memory(&meminfo))
        .ok_or_else(|| internal("cannot read /proc/meminfo"))
}

pub fn services(node: &Node, args: &Args) -> Result<Answer> {
    match system::init() {
        Init::Systemd => systemd_units(node, args),
        Init::Procd => system::procd_units(node, args.string("state"), args.string("pattern")),
        Init::None => Err(system::no_init()),
    }
    .map(Answer::of)
}

fn systemd_units(node: &Node, args: &Args) -> Result<Value> {
    let mut argv = owned(&["systemctl", "list-units", "--no-legend", "--plain", "--no-pager"]);
    if let Some(unit_type) = args.string("type").filter(|unit_type| *unit_type != "all") {
        argv.push(format!("--type={unit_type}"));
    }
    match args.string("state") {
        None => {}
        Some("all") => argv.push("--all".into()),
        Some("inactive") => argv.extend(owned(&["--all", "--state=inactive"])),
        Some(state) => argv.push(format!("--state={state}")),
    }
    if let Some(pattern) = args.string("pattern") {
        argv.extend(owned(&["--", pattern]));
    }
    Ok(parsers::units(&node.exec_ok(&borrowed(&argv))?, &node.redactor))
}

pub fn service(node: &Node, args: &Args) -> Result<Answer> {
    let name = args.string("name").unwrap_or_default();
    let lines = usize_arg(args, "lines", 20, 0)?;
    match system::init() {
        Init::Systemd => systemd_service(node, name, lines),
        Init::Procd => system::procd_service(node, name.trim_end_matches(".service"), lines),
        Init::None => Err(system::no_init()),
    }
    .map(Answer::of)
}

/// A unit (a service when [name] says no type) and its last [lines] in the journal.
fn systemd_service(node: &Node, name: &str, lines: usize) -> Result<Value> {
    let unit = if name.contains('.') { name.to_string() } else { format!("{name}.service") };
    let properties = parsers::key_values(&node.exec_ok(&[
        "systemctl",
        "show",
        "--no-pager",
        "--property=Id,Description,LoadState,ActiveState,SubState,Result,UnitFileState,FragmentPath,MainPID,\
         ExecMainStatus,NRestarts,MemoryCurrent,ActiveEnterTimestamp,StateChangeTimestamp,Type,Restart",
        "--",
        &unit,
    ])?);
    if properties.get("LoadState").map(String::as_str) == Some("not-found") {
        return Err(error(ErrorCode::NotFound, format!("no unit named {unit}")));
    }
    let journal = unit_journal(node, &unit, lines)?;
    let mut service = parsers::unit(&properties, &node.redactor);
    service["journal"] = json!(journal);
    Ok(service)
}

fn unit_journal(node: &Node, unit: &str, lines: usize) -> Result<Vec<Value>> {
    if lines == 0 {
        return Ok(vec![]);
    }
    let count = lines.to_string();
    let journal = node.exec_ok(&["journalctl", "-u", unit, "-n", &count, "-o", "json", "--no-pager", "-q"])?;
    Ok(parsers::journal(&journal, &node.redactor))
}

pub fn containers(node: &Node, args: &Args) -> Result<Answer> {
    let include_stopped = args.bool("all").unwrap_or(true);
    let summaries: Vec<Value> = inspect_all(node)?
        .iter()
        .map(parsers::container_summary)
        .filter(|summary| include_stopped || summary["state"] == "running")
        .collect();
    Ok(Answer::of(json!(summaries)))
}

pub fn container(node: &Node, args: &Args) -> Result<Answer> {
    let name = args.string("name").unwrap_or_default();
    let inspected = node.exec(&["docker", "inspect", "--type", "container", "--", name], MINUTE)?;
    if inspected.exit_code != 0 {
        let message = inspected.err();
        if message.contains("No such") {
            return Err(no_container(name));
        }
        return Err(docker_error(message.trim()));
    }
    let found: Vec<Map<String, Value>> = serde_json::from_str(&inspected.out()).unwrap_or_default();
    let container = found.into_iter().next().ok_or_else(|| no_container(name))?;
    let digests = container.get("Image").and_then(Value::as_str).map(|image| repo_digests(node, image));
    Ok(Answer::of(parsers::container_detail(&container, &digests.unwrap_or_default(), &node.redactor)))
}

/// The registry digests of [image]; none when Docker can't tell.
fn repo_digests(node: &Node, image: &str) -> Vec<String> {
    node.exec(&["docker", "image", "inspect", "--format", "{{json .RepoDigests}}", "--", image], MINUTE)
        .ok()
        .filter(|inspected| inspected.exit_code == 0)
        .and_then(|inspected| serde_json::from_str::<Vec<String>>(inspected.out().trim()).ok())
        .unwrap_or_default()
}

/// `docker inspect` of every container, running or not.
fn inspect_all(node: &Node) -> Result<Vec<Map<String, Value>>> {
    let listed = node.exec(&["docker", "ps", "-aq", "--no-trunc"], MINUTE)?;
    if listed.exit_code != 0 {
        return Err(docker_error(listed.err().trim()));
    }
    let listed_ids = listed.out();
    let ids: Vec<&str> = listed_ids.lines().map(str::trim).filter(|id| !id.is_empty()).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut argv = vec!["docker", "inspect", "--type", "container"];
    argv.extend(&ids);
    let inspected = node.exec(&argv, MINUTE)?;
    // A container removed since `ps` fails the command, and the rest are still printed.
    if inspected.exit_code != 0 && inspected.out().trim().is_empty() {
        return Err(docker_error(inspected.err().trim()));
    }
    Ok(serde_json::from_str(&inspected.out()).unwrap_or_default())
}

fn docker_error(message: &str) -> LimenError {
    let last_line = message.lines().last().unwrap_or("failed");
    if message.contains("Cannot connect") || message.contains("permission denied") {
        error(ErrorCode::Unavailable, format!("docker: {last_line}"))
    } else {
        internal(format!("docker: {last_line}"))
    }
}

fn no_container(name: &str) -> LimenError {
    error(ErrorCode::NotFound, format!("no container named {name}"))
}

pub fn logs(node: &Node, args: &Args) -> Result<Answer> {
    let source = args.string("source").unwrap_or_default();
    let name = args.string("name");
    let requested = usize_arg(args, "lines", 200, 1)?;
    let lines = requested.min(node.config.max_lines);
    let filter = LogFilter {
        source: None,
        priority: args.string("priority"),
        since: instant_arg(node, args, "since")?,
        until: instant_arg(node, args, "until")?,
        grep: args.string("grep"),
    };
    let answer = match source {
        "unit" | "journal" => system_logs(node, source, name, lines, &filter)?,
        "container" => container_logs(node, name, lines, &filter)?,
        "file" => file_logs(node, name, lines, &filter)?,
        other => return Err(bad_request(format!("unknown source {other}"))),
    };
    let clamped = lines < requested;
    Ok(if clamped { Answer::cut(answer.data, true) } else { answer })
}

/// `source = unit` or `journal`: systemd's journal, or OpenWrt's logread.
fn system_logs(node: &Node, source: &str, name: Option<&str>, lines: usize, filter: &LogFilter) -> Result<Answer> {
    match (source, name) {
        ("journal", Some(_)) => return Err(bad_request("source journal takes no name")),
        ("unit", None) => return Err(bad_request("source unit needs a name")),
        _ => {}
    }
    match system::init() {
        Init::None => Err(error(ErrorCode::Unavailable, "no journal or logread on this node")),
        Init::Procd => {
            let program = name.map(|unit| unit.trim_end_matches(".service"));
            let entries = system::logread(node, lines, &LogFilter { source: program, ..*filter })?;
            Ok(Answer::of(json!(entries)))
        }
        Init::Systemd => journal(node, lines, &LogFilter { source: name, ..*filter }),
    }
}

/// The journal, of the unit [filter] names as its source, or of the whole system.
fn journal(node: &Node, lines: usize, filter: &LogFilter) -> Result<Answer> {
    let argv = journalctl_argv(node, lines, filter)?;
    let result = node.exec(&borrowed(&argv), MINUTE)?;
    // journalctl --grep exits 1 when nothing matches: that is an empty answer, not an error.
    let output = result.out();
    if result.exit_code != 0 && output.trim().is_empty() && !result.err().trim().is_empty() {
        return Err(internal(format!("journalctl: {}", result.failure_reason())));
    }
    let entries = parsers::journal(&output, &node.redactor);
    Ok(Answer::cut(json!(last_matching_messages(entries, filter.grep, lines)), result.truncated))
}

fn journalctl_argv(node: &Node, lines: usize, filter: &LogFilter) -> Result<Vec<String>> {
    let mut argv = owned(&["journalctl", "-o", "json", "--no-pager", "-q", "-n"]);
    argv.push(scan_window(node, lines, filter.grep).to_string());
    if let Some(unit) = filter.source {
        if !matches_whole(UNIT, unit) {
            return Err(bad_request(format!("'{unit}' is not a unit name")));
        }
        argv.extend(owned(&["-u", unit]));
    }
    if let Some(since) = filter.since {
        argv.push(format!("--since={}", journal_time(since)));
    }
    if let Some(until) = filter.until {
        argv.push(format!("--until={}", journal_time(until)));
    }
    if let Some(priority) = filter.priority {
        argv.push(format!("--priority={priority}"));
    }
    if let Some(grep) = filter.grep {
        argv.extend([format!("--grep={}", pcre_literal(grep)), "--case-sensitive=false".into()]);
    }
    Ok(argv)
}

/// How many lines to read for the last [lines] that match [grep]. With grep the program only narrows the search: the
/// lines are matched again once redacted, and those that matched only what redaction hid must not take the place of
/// the rest, so the window is `scan_lines`, not `lines`.
fn scan_window(node: &Node, lines: usize, grep: Option<&str>) -> usize {
    if grep.is_none() { lines } else { node.config.scan_lines }
}

/// The last [lines] of [entries] whose (redacted) text holds [grep], case aside. Matching after redaction, never
/// before: otherwise a guess at a secret, one character at a time, is told apart by whether a line comes back.
pub fn last_matching<T>(entries: Vec<T>, grep: Option<&str>, lines: usize, text: impl Fn(&T) -> &str) -> Vec<T> {
    let grep = grep.map(str::to_lowercase);
    let mut matched: Vec<T> = entries
        .into_iter()
        .filter(|entry| grep.as_ref().is_none_or(|grep| text(entry).to_lowercase().contains(grep)))
        .collect();
    let first_kept = matched.len().saturating_sub(lines);
    matched.split_off(first_kept)
}

/// [last_matching] of log rows, by their `message`.
pub fn last_matching_messages(rows: Vec<Value>, grep: Option<&str>, lines: usize) -> Vec<Value> {
    last_matching(rows, grep, lines, |row| row["message"].as_str().unwrap_or(""))
}

/// `source = container`: what the container wrote to stdout and stderr, in the order it wrote it.
fn container_logs(node: &Node, name: Option<&str>, lines: usize, filter: &LogFilter) -> Result<Answer> {
    let name = required_name("container", name)?;
    if !matches_whole(CONTAINER, name) {
        return Err(bad_request(format!("'{name}' is not a container name")));
    }
    let window = scan_window(node, lines, filter.grep).to_string();
    let mut argv = owned(&["docker", "logs", "--timestamps", "--tail", &window]);
    if let Some(since) = filter.since {
        argv.push(format!("--since={}", iso(since)));
    }
    if let Some(until) = filter.until {
        argv.push(format!("--until={}", iso(until)));
    }
    argv.extend(owned(&["--", name]));
    let result = node.exec(&borrowed(&argv), MINUTE)?;
    if result.exit_code != 0 {
        let stderr = result.err();
        if stderr.contains("No such container") {
            return Err(no_container(name));
        }
        return Err(docker_error(stderr.trim()));
    }
    let mut merged = parsers::docker_log_lines(&result.out(), "stdout");
    merged.extend(parsers::docker_log_lines(&result.err(), "stderr"));
    merged.sort_by(|(first_time, ..), (second_time, ..)| first_time.cmp(second_time));
    let rows = merged
        .iter()
        .map(
            |(time, stream, message)| json!({"time": time, "stream": stream, "message": node.redactor.redact(message)}),
        )
        .collect();
    Ok(Answer::cut(json!(last_matching_messages(rows, filter.grep, lines)), result.truncated))
}

/// `source = file`: the end of a file the policy lets the client read.
fn file_logs(node: &Node, name: Option<&str>, lines: usize, filter: &LogFilter) -> Result<Answer> {
    if filter.since.is_some() || filter.until.is_some() {
        return Err(bad_request("since and until do not apply to files"));
    }
    let opened = allowed_file(node, required_name("file", name)?)?;
    let path = &opened.info.path;
    let window = scan_window(node, lines, filter.grep);
    let tail = fs::tail(&opened.file, window, (window as u64 * LOG_BYTES_PER_LINE).min(MAX_SCAN_BYTES))
        .map_err(|reason| internal(format!("cannot read {path}: {reason}")))?;
    if tail.binary {
        return Err(bad_request(format!("{path} is binary")));
    }
    // Redacted before it is cut into lines: a private key the window holds whole spans several of them.
    let redacted = node.redactor.redact(&tail.lines.join("\n"));
    let matched = last_matching(redacted.split('\n').collect(), filter.grep, lines, |line| line);
    Ok(Answer::of(json!({"path": path, "lines": matched})))
}

fn required_name<'a>(source: &str, name: Option<&'a str>) -> Result<&'a str> {
    name.ok_or_else(|| bad_request(format!("source {source} needs a name")))
}

pub fn read_file(node: &Node, args: &Args) -> Result<Answer> {
    let opened = allowed_file(node, args.string("path").unwrap_or_default())?;
    let (path, size) = (&opened.info.path, opened.info.size);
    let from = usize_arg(args, "from", 1, 1)?;
    let count = usize_arg(args, "lines", 500, 1)?;
    let slice = fs::read_lines(&opened.file, from, count, node.config.max_file_bytes, MAX_SKIP_BYTES)
        .map_err(|reason| bad_request(format!("{path}: {reason}")))?;
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
    let requested = args.string("path").unwrap_or_default();
    let dir = resolve(node, requested, &|path| visible(node, path))?;
    let opened = fs::open_dir_exact(&dir).map_err(|failure| match failure {
        fs::OpenError::Missing => does_not_exist(requested),
        fs::OpenError::NotDirectory => bad_request(format!("{requested} is not a directory")),
        fs::OpenError::Other(reason) => internal(reason),
    })?;
    // Read once for the whole listing, not per entry.
    let (users, groups) = (IdNames::users(), IdNames::groups());
    let mut entries: Vec<Value> = opened
        .entries()
        .map_err(internal)?
        .iter()
        .filter_map(|(name, entry)| listed_entry(node, &users, &groups, name, entry))
        .take(MAX_LISTED_ENTRIES + 1)
        .collect();
    let truncated = entries.len() > MAX_LISTED_ENTRIES;
    entries.truncate(MAX_LISTED_ENTRIES);
    Ok(Answer::cut(json!({"path": dir, "entries": entries}), truncated))
}

/// [entry] as `list_dir` shows it; none when the client may not see it. A link is shown where it leads, if that is
/// somewhere the client may see.
fn listed_entry(node: &Node, users: &IdNames, groups: &IdNames, name: &str, entry: &fs::FileInfo) -> Option<Value> {
    let (target, kind) = match entry.kind {
        fs::FileType::Link => match walk(node, &entry.path) {
            Walked::Found(target) => {
                let kind = fs::stat(&target).map(|info| info.kind);
                (target, kind)
            }
            Walked::Missing(_) | Walked::Hidden => return None,
        },
        kind => (entry.path.clone(), Some(kind)),
    };
    let shown =
        node.policy.allowed(&target) || (kind == Some(fs::FileType::Directory) && node.policy.leads_to(&target));
    if !shown {
        return None;
    }
    let mut listed = json!({
        "name": name,
        "type": entry.kind.wire(),
        "size_bytes": entry.size,
        "mode": format!("{:04o}", entry.mode & 0o7777),
        "owner": users.name(entry.uid),
        "group": groups.name(entry.gid),
        "modified": iso(entry.modified),
    });
    if entry.kind == fs::FileType::Link {
        listed["target"] = json!(target);
    }
    Some(listed)
}

pub fn processes(node: &Node, args: &Args) -> Result<Answer> {
    let limit = usize_arg(args, "limit", 20, 1)?;
    Ok(Answer::of(system::processes(node, args.string("sort") == Some("memory"), limit)))
}

pub fn ports() -> Answer {
    Answer::of(system::ports())
}

pub fn history(node: &Node, args: &Args) -> Result<Answer> {
    let count = usize_arg(args, "lines", 50, 1)?;
    if !fs::exists(&node.config.audit) {
        return Ok(Answer::of(json!([])));
    }
    let audit = fs::open_read(&node.config.audit).map_err(internal)?;
    let tail = fs::tail(&audit, count, count as u64 * AUDIT_BYTES_PER_LINE).map_err(internal)?;
    // The log keeps every argument as it came, deploy ones included; what leaves the node is redacted.
    let entries: Vec<Value> =
        tail.lines.iter().filter_map(|line| serde_json::from_str(&node.redactor.redact(line)).ok()).collect();
    Ok(Answer::of(json!(entries)))
}

pub fn check(node: &Node, args: &Args) -> Result<Answer> {
    let (entry, spec) = node_scripts::find(node, ScriptKind::Check, args.string("name").unwrap_or_default())?;
    let env = node_scripts::environment(&spec, &args.object("args"))?;
    super::gate::allow(Duration::from_secs(spec.timeout_seconds) + MINUTE);
    let result = node_scripts::run(&entry, &spec, env, node_scripts::MAX_OUTPUT_BYTES, None)?;
    if result.timed_out {
        return Err(error(
            ErrorCode::Timeout,
            format!("check {} did not finish in {}s", spec.name, spec.timeout_seconds),
        ));
    }
    let output = node.redactor.redact(&result.out());
    let output = output.trim_end();
    let (summary, detail) = output.split_once('\n').unwrap_or((output, ""));
    let mut answer =
        json!({"check": spec.name, "status": check_status(result.exit_code), "exit_code": result.exit_code});
    if let Some(signal) = result.signal {
        answer["signal"] = json!(signal);
    }
    answer["summary"] = json!(summary);
    answer["detail"] = json!(detail);
    let stderr = result.err();
    if !stderr.trim().is_empty() {
        answer["stderr"] = json!(last_chars(node.redactor.redact(&stderr).trim_end(), MAX_CHECK_STDERR_CHARS));
    }
    Ok(Answer::cut(answer, result.truncated))
}

/// The Nagios plugin convention (spec §6).
fn check_status(exit_code: i32) -> &'static str {
    match exit_code {
        0 => "ok",
        1 => "warn",
        2 => "fail",
        _ => "unknown",
    }
}

fn last_chars(text: &str, count: usize) -> String {
    let skipped = text.chars().count().saturating_sub(count);
    text.chars().skip(skipped).collect()
}

/// [requested] opened for reading, once: what is checked —the policy, the type, hard links, keys— is checked on what
/// is then read.
pub fn allowed_file(node: &Node, requested: &str) -> Result<fs::Opened> {
    let resolved = resolve(node, requested, &|path| node.policy.allowed(path))?;
    let opened = fs::open_exact(&resolved).map_err(|failure| match failure {
        fs::OpenError::Missing => does_not_exist(requested),
        fs::OpenError::NotDirectory | fs::OpenError::Other(_) => internal(format!("cannot open {requested}")),
    })?;
    if opened.info.kind != fs::FileType::File {
        return Err(bad_request(format!("{requested} is not a regular file")));
    }
    // Another name for the same file could be anywhere, a denied place included: a hard link into an allowed
    // directory would carry the policy with it.
    if opened.info.links > 1 {
        return Err(error(ErrorCode::Denied, format!("{requested} is not readable: it has other hard links")));
    }
    // A window of lines can fall between a key's markers, where redaction can't see it. Keys live in small files, so
    // those are searched whole; in a large one, a log, the window is redacted as a whole instead.
    if opened.info.size <= KEY_FILE_MAX_BYTES && fs::contains(&opened.file, PRIVATE_KEY, KEY_FILE_MAX_BYTES) {
        return Err(error(ErrorCode::Denied, format!("{requested} holds a private key")));
    }
    Ok(opened)
}

/// Where a path leads, as far as the client may know.
enum Walked {
    Found(String),
    /// It doesn't exist; this is where it would be.
    Missing(String),
    /// On the way it passes somewhere the policy hides.
    Hidden,
}

/// [path] walked one component at a time, as the kernel does: links followed, `..` taken from where the walk is,
/// not from the text. Every step must be allowed or lead to something allowed, so nothing is learnt of a place the
/// policy hides, not even whether it exists. Past a missing component the walk goes on as text, judged the same way.
fn walk(node: &Node, path: &str) -> Walked {
    let mut pending = components_last_first(path);
    let mut current = String::from("/");
    let mut missing = false;
    let mut links_followed = 0;
    while let Some(component) = pending.pop() {
        match component.as_str() {
            "." => continue,
            ".." => {
                current = fs::parent(&current).into();
                continue;
            }
            _ => {}
        }
        let next = if current == "/" { format!("/{component}") } else { format!("{current}/{component}") };
        if !visible(node, &next) {
            return Walked::Hidden;
        }
        if !missing {
            match fs::lstat(&next) {
                None => missing = true,
                Some(info) if info.kind == fs::FileType::Link => {
                    links_followed += 1;
                    let Some(target) = fs::read_link(&next).filter(|_| links_followed <= MAX_LINKS) else {
                        return Walked::Hidden;
                    };
                    if target.starts_with('/') {
                        current = "/".into();
                    }
                    pending.extend(components_last_first(&target));
                    continue;
                }
                Some(_) => {}
            }
        }
        current = next;
    }
    if missing { Walked::Missing(current) } else { Walked::Found(current) }
}

/// The components of [path], last first, for the walk to pop.
fn components_last_first(path: &str) -> Vec<String> {
    path.split('/').rev().filter(|component| !component.is_empty()).map(String::from).collect()
}

/// Whether the client may see [path]: it is allowed, or on the way to something allowed.
fn visible(node: &Node, path: &str) -> bool {
    node.policy.allowed(path) || node.policy.leads_to(path)
}

/// [requested] walked (see [walk]) and judged by [permits]. A refusal names the path as it was asked for and says no
/// more: the reason would tell where it leads.
fn resolve(node: &Node, requested: &str, permits: &dyn Fn(&str) -> bool) -> Result<String> {
    if !requested.starts_with('/') {
        return Err(bad_request(format!("{requested} is not an absolute path")));
    }
    match walk(node, requested) {
        Walked::Found(path) if permits(&path) => Ok(path),
        Walked::Missing(path) if permits(&path) => Err(does_not_exist(requested)),
        _ => Err(error(ErrorCode::Denied, format!("{requested} is not readable"))),
    }
}

fn does_not_exist(requested: &str) -> LimenError {
    error(ErrorCode::NotFound, format!("{requested} does not exist"))
}

/// The integer argument [name], [default] when absent, and never below [min].
fn usize_arg(args: &Args, name: &str, default: i32, min: i32) -> Result<usize> {
    Ok(args.small_integer(name)?.unwrap_or(default).max(min) as usize)
}

fn instant_arg(node: &Node, args: &Args, name: &str) -> Result<Option<i64>> {
    args.string(name).map(|text| instant(node, text)).transpose()
}

/// `30m` (that long ago) or `2026-09-26T08:00Z`.
fn instant(node: &Node, text: &str) -> Result<i64> {
    if let Some(ago) = durations::parse(text) {
        return Ok(node.now() - ago.as_secs() as i64);
    }
    parse_iso(text).ok_or_else(|| bad_request(format!("'{text}' is not a time")))
}

fn journal_time(epoch: i64) -> String {
    format!("{} UTC", iso(epoch).replace('T', " ").trim_end_matches('Z'))
}

/// [text] as a PCRE pattern that matches only itself: every non-alphanumeric character escaped.
pub fn pcre_literal(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_alphanumeric() || character == ' ' {
                character.to_string()
            } else {
                format!("\\{character}")
            }
        })
        .collect()
}

/// Whether all of [text] matches [pattern].
fn matches_whole(pattern: &str, text: &str) -> bool {
    full_match(pattern).is_ok_and(|regex| regex.is_match(text))
}

/// [argv] with every argument owned, to add the ones built with `format!`.
fn owned(argv: &[&str]) -> Vec<String> {
    argv.iter().map(ToString::to_string).collect()
}

/// [argv] as [Node::exec] takes it.
fn borrowed(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
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
        for subdir in ["etc/private", "outside"] {
            std::fs::create_dir_all(format!("{dir}/{subdir}")).unwrap();
        }
        let write = |file: &str, text: &str| std::fs::write(format!("{dir}/{file}"), text).unwrap();
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

    fn path_args(path: &str) -> Args {
        args_of(json!({"path": path}))
    }

    fn args_of(value: Value) -> Args {
        match value {
            Value::Object(args) => args,
            other => panic!("{other} is not an object"),
        }
    }

    fn refused(result: Result<Answer>) -> ErrorCode {
        result.err().expect("refused").code
    }

    #[test]
    fn an_allowed_file_is_read_and_redacted() {
        let tree = tree();
        let answer = read_file(&tree.node, &path_args(&format!("{}/etc/app.conf", tree.dir))).unwrap();
        assert_eq!(answer.data["content"], "name = app\npassword = \"[redacted]\"");
        assert_eq!(answer.data["to"], 2);
    }

    #[test]
    fn a_denied_path_says_nothing_of_its_existence() {
        let tree = tree();
        for denied in [
            "private/real",
            "private/missing",
            "../outside/real",
            "../outside/missing",
            "link",
            "privlink/real",
            "privlink/missing",
        ] {
            let path = format!("{}/etc/{denied}", tree.dir);
            assert_eq!(refused(read_file(&tree.node, &path_args(&path))), ErrorCode::Denied, "read_file {denied}");
            assert_eq!(refused(list_dir(&tree.node, &path_args(&path))), ErrorCode::Denied, "list_dir {denied}");
        }
        let missing = format!("{}/etc/missing", tree.dir);
        assert_eq!(refused(read_file(&tree.node, &path_args(&missing))), ErrorCode::NotFound);
        let file = format!("{}/etc/app.conf", tree.dir);
        assert_eq!(refused(list_dir(&tree.node, &path_args(&file))), ErrorCode::BadRequest);
    }

    #[test]
    fn dots_are_walked_where_links_lead_not_as_text() {
        // As text, etc/up/../x is etc/x, allowed; the kernel reads outside/x, which is not. Whether it exists must not
        // show either.
        let tree = tree();
        std::fs::create_dir_all(format!("{}/outside/sub", tree.dir)).unwrap();
        symlink(format!("{}/outside/sub", tree.dir), format!("{}/etc/up", tree.dir)).unwrap();
        for relative in ["up/../real", "up/../missing", "privlink/../../outside/real"] {
            let path = format!("{}/etc/{relative}", tree.dir);
            let refusal = read_file(&tree.node, &path_args(&path)).err().expect("refused");
            assert_eq!((refusal.code, refusal.message), (ErrorCode::Denied, format!("{path} is not readable")));
        }
        // Where it does lead somewhere allowed, it is read.
        let back = format!("{}/etc/private/../app.conf", tree.dir);
        assert!(read_file(&tree.node, &path_args(&back)).is_err(), "a step through a denied directory is refused");
        let fine = format!("{}/etc/./app.conf", tree.dir);
        assert_eq!(
            read_file(&tree.node, &path_args(&fine)).unwrap().data["path"],
            format!("{}/etc/app.conf", tree.dir)
        );
    }

    #[test]
    fn a_hard_link_is_not_read() {
        let tree = tree();
        std::fs::hard_link(format!("{}/outside/real", tree.dir), format!("{}/etc/hard", tree.dir)).unwrap();
        assert_eq!(refused(read_file(&tree.node, &path_args(&format!("{}/etc/hard", tree.dir)))), ErrorCode::Denied);
    }

    #[test]
    fn limens_own_logs_are_never_read() {
        let tree = tree();
        let node = Node::new(NodeConfig {
            allow: vec![format!("{}/**", tree.dir)],
            audit: format!("{}/etc/audit.jsonl", tree.dir),
            ..Default::default()
        });
        std::fs::write(&node.config.audit, "{}\n").unwrap();
        assert_eq!(refused(read_file(&node, &path_args(&node.config.audit))), ErrorCode::Denied);
    }

    #[test]
    fn grep_sees_only_what_redaction_left() {
        let tree = tree();
        let log = format!("{}/etc/app.log", tree.dir);
        std::fs::write(&log, "login ok\npassword=hunter2\n").unwrap();
        let matching_lines = |grep: &str| {
            let args = args_of(json!({"source": "file", "name": &log, "grep": grep}));
            logs(&tree.node, &args).unwrap().data["lines"].as_array().unwrap().len()
        };
        assert_eq!(matching_lines("password=h"), 0, "a guess at the secret is not told apart");
        assert_eq!(matching_lines("password=[redacted]"), 1);
        let rows = vec![json!({"message": "token=[redacted]"}), json!({"message": "a"}), json!({"message": "A"})];
        let last = last_matching(rows, Some("a"), 1, |row| row["message"].as_str().unwrap());
        assert_eq!(last, [json!({"message": "A"})]);
    }

    #[test]
    fn listing_hides_what_cannot_be_read() {
        let tree = tree();
        let answer = list_dir(&tree.node, &path_args(&format!("{}/etc", tree.dir))).unwrap();
        let names: Vec<&str> =
            answer.data["entries"].as_array().unwrap().iter().map(|entry| entry["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"app.conf"), "{names:?}");
        assert!(!names.contains(&"private") && !names.contains(&"link"), "{names:?}");
    }

    #[test]
    fn a_file_holding_a_private_key_is_not_read_at_all() {
        // A window of lines between the markers would carry the key's body past redaction.
        let tree = tree();
        let window = args_of(json!({"path": format!("{}/etc/deploy.pem", tree.dir), "from": 2, "lines": 2}));
        assert_eq!(refused(read_file(&tree.node, &window)), ErrorCode::Denied);
    }

    #[test]
    fn a_key_in_a_big_log_is_masked_when_the_window_holds_it() {
        // Over the size searched whole for keys: the window is redacted as one text, markers and body together.
        let tree = tree();
        let filler = format!("{}\n", "x".repeat(99)).repeat(12_000);
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        std::fs::write(format!("{}/etc/big.log", tree.dir), format!("{filler}{key}done\n")).unwrap();
        let args = args_of(json!({"source": "file", "name": format!("{}/etc/big.log", tree.dir), "lines": 10}));
        let lines = logs(&tree.node, &args).unwrap().data.to_string();
        assert!(lines.contains("done"), "{lines}");
        assert!(!lines.contains("b3BlbnNzaA"), "{lines}");
    }

    #[test]
    fn history_is_redacted() {
        let tree = tree();
        std::fs::write(
            &tree.node.config.audit,
            "{\"request\":\"action\",\"args\":{\"name\":\"rotate\",\"args\":{\"token\":\"abc123\"}}}\n",
        )
        .unwrap();
        let history = history(&tree.node, &Map::new()).unwrap().data.to_string();
        assert!(history.contains("rotate"), "{history}");
        assert!(!history.contains("abc123"), "{history}");
    }

    #[test]
    fn files_that_are_not_scripts_are_ignored_not_problems() {
        // A README, a .gitkeep to keep the folder in git: neither may stop `apply` or show up as a broken script.
        let tree = tree();
        let dir = format!("{}/setup", tree.dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in ["README.md", ".gitkeep"] {
            std::fs::write(format!("{dir}/{file}"), "x\n").unwrap();
        }
        let node = Node::new(NodeConfig {
            explicit_checks: Some(dir.clone()),
            explicit_setup: Some(dir),
            ..Default::default()
        });
        let entries = node_scripts::discover(&node, ScriptKind::Setup);
        assert_eq!(entries.iter().map(|entry| entry.file.as_str()).collect::<Vec<_>>(), ["README.md"]);
        assert!(entries[0].ignored);
        assert!(node_scripts::catalog(&node).problems.is_empty());
    }

    #[test]
    fn a_literal_for_journalctl_grep() {
        assert_eq!(pcre_literal("a.b (c)"), "a\\.b \\(c\\)");
    }
}
