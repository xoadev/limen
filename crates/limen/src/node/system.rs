//! What differs between the machines limen runs on (spec §11): the init system and its log, and OpenWrt. What the
//! kernel says the same way everywhere —processes, sockets, mounts— is read from `/proc` directly, never through `ps`,
//! `ss` or `df`, whose busybox versions lack the options.

use super::read::last_matching_messages;
use super::{MINUTE, Node};
use crate::os::{fs, sys};
use limen_core::glob::segment_matches;
use limen_core::protocol::{ErrorCode, LimenError, Result, error};
use limen_core::redactor::Redactor;
use limen_core::system::parsers;
use limen_core::system::procfs::{self, LogreadEntry, Mount, NetSocket, ProcSample};
use limen_core::time;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Read;

/// Where OpenWrt says which board it runs on.
const SYSINFO_DIR: &str = "/tmp/sysinfo";
const MAX_BOARD_BYTES: u64 = 4096;
const MAX_CMDLINE_BYTES: usize = 8192;
/// What `processes` shows of a command line.
const MAX_COMMAND_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Procd,
    None,
}

impl Init {
    pub fn wire(self) -> &'static str {
        match self {
            Init::Systemd => "systemd",
            Init::Procd => "procd",
            Init::None => "none",
        }
    }
}

pub fn openwrt() -> bool {
    fs::exists("/etc/openwrt_release")
}

pub fn init() -> Init {
    if fs::exists("/run/systemd/system") {
        Init::Systemd
    } else if openwrt() {
        Init::Procd
    } else {
        Init::None
    }
}

/// The answer to a request about services on a node with neither init system.
pub fn no_init() -> LimenError {
    error(ErrorCode::Unavailable, "no systemd or procd on this node")
}

/// The board, where the system says it: OpenWrt, in `/tmp`. Only a small regular file root wrote, in a directory
/// only root writes: anywhere else, anyone could put there a FIFO to hang the request, or a link to another file.
pub fn board() -> Option<String> {
    if !openwrt() {
        return None;
    }
    let dir = fs::lstat(SYSINFO_DIR)
        .filter(|info| info.kind == fs::FileType::Directory && info.uid == 0 && info.mode & 0o022 == 0)?;
    let opened = fs::open_exact(&format!("{}/model", dir.path)).ok()?;
    let info = &opened.info;
    if info.kind != fs::FileType::File || info.uid != 0 || info.links != 1 || info.size > MAX_BOARD_BYTES {
        return None;
    }
    let mut text = String::new();
    (&opened.file).take(MAX_BOARD_BYTES).read_to_string(&mut text).ok()?;
    let board = text.lines().next().unwrap_or("").trim().to_string();
    (!board.is_empty()).then_some(board)
}

pub fn disks() -> Result<Value> {
    let text =
        fs::read_text("/proc/self/mounts").ok_or_else(|| error(ErrorCode::Unavailable, "no /proc/self/mounts"))?;
    Ok(Value::Array(procfs::mounts(&text).iter().filter_map(disk).collect()))
}

/// The space of the filesystem at [mount], if it is one that has any.
fn disk(mount: &Mount) -> Option<Value> {
    // A file bind-mounted over another (a container's /etc/hostname) is not a filesystem to report.
    if !fs::is_directory(&mount.point) {
        return None;
    }
    let (free, total) = fs::space(&mount.point).filter(|(_, total)| *total > 0)?;
    let used = total - free;
    Some(json!({
        "mount": mount.point,
        "device": mount.device,
        "fstype": mount.kind,
        "size_bytes": total,
        "used_bytes": used,
        "available_bytes": free,
        "used_percent": (used * 1000 / total) as f64 / 10.0,
    }))
}

pub fn uptime_seconds() -> Option<f64> {
    fs::read_text("/proc/uptime")?.split(' ').next()?.parse().ok()
}

fn memory_total_kb() -> Option<u64> {
    let meminfo = fs::read_text("/proc/meminfo")?;
    meminfo.lines().find(|line| line.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()
}

fn pids() -> Vec<u32> {
    fs::list("/proc").unwrap_or_default().iter().filter_map(|name| name.parse().ok()).collect()
}

fn sample(pid: u32) -> Option<ProcSample> {
    let stat = fs::read_text(&format!("/proc/{pid}/stat"))?;
    let status = fs::read_text(&format!("/proc/{pid}/status"))?;
    let cmdline = fs::read(&format!("/proc/{pid}/cmdline"), MAX_CMDLINE_BYTES)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    procfs::sample(pid, &stat, &status, &cmdline)
}

/// Names by numeric id —of users, or of groups—, read once for a whole listing.
pub struct IdNames(BTreeMap<u32, String>);

impl IdNames {
    pub fn users() -> Self {
        IdNames(fs::accounts().into_iter().map(|account| (account.uid, account.name)).collect())
    }

    pub fn groups() -> Self {
        IdNames(fs::groups())
    }

    /// The name of [id], or the number itself when it has none.
    pub fn name(&self, id: u32) -> String {
        self.0.get(&id).cloned().unwrap_or_else(|| id.to_string())
    }
}

pub fn processes(node: &Node, by_memory: bool, limit: usize) -> Value {
    let machine = Machine::read();
    let mut samples: Vec<ProcSample> = pids().into_iter().filter_map(sample).collect();
    if by_memory {
        samples.sort_by_key(|sample| std::cmp::Reverse(sample.rss_kb));
    } else {
        samples.sort_by(|first, second| machine.cpu_percent(second).total_cmp(&machine.cpu_percent(first)));
    }
    Value::Array(samples.iter().take(limit).map(|sample| machine.process_row(sample, &node.redactor)).collect())
}

/// What the figures of a process are measured against, read once for the whole listing.
struct Machine {
    uptime_seconds: f64,
    ticks_per_second: u64,
    memory_total_kb: u64,
    users: IdNames,
}

impl Machine {
    fn read() -> Self {
        Machine {
            uptime_seconds: uptime_seconds().unwrap_or(0.0),
            memory_total_kb: memory_total_kb().unwrap_or(0),
            ticks_per_second: sys::ticks_per_second(),
            users: IdNames::users(),
        }
    }

    fn cpu_percent(&self, sample: &ProcSample) -> f64 {
        procfs::cpu_percent(sample, self.uptime_seconds, self.ticks_per_second)
    }

    fn process_row(&self, sample: &ProcSample, redactor: &Redactor) -> Value {
        let started_seconds = sample.start_ticks as f64 / self.ticks_per_second as f64;
        let memory_percent =
            (self.memory_total_kb > 0).then(|| (sample.rss_kb * 1000 / self.memory_total_kb) as f64 / 10.0);
        json!({
            "pid": sample.pid,
            "user": sample.uid.map(|uid| self.users.name(uid)),
            "cpu_percent": self.cpu_percent(sample),
            "memory_percent": memory_percent,
            "rss_bytes": sample.rss_kb * 1024,
            "elapsed_seconds": (self.uptime_seconds - started_seconds).max(0.0) as i64,
            "command": redactor.redact(&sample.cmdline).chars().take(MAX_COMMAND_CHARS).collect::<String>(),
        })
    }
}

/// The processes holding each socket, by inode, as (pid, name).
type SocketOwners = BTreeMap<u64, Vec<(u32, String)>>;

pub fn ports() -> Value {
    let sockets = listening_sockets();
    let owners = socket_owners();
    Value::Array(sockets.iter().map(|socket| port_row(socket, &owners)).collect())
}

fn listening_sockets() -> Vec<NetSocket> {
    let mut sockets: Vec<NetSocket> = ["tcp", "tcp6", "udp", "udp6"]
        .iter()
        .flat_map(|protocol| {
            fs::read_text(&format!("/proc/net/{protocol}"))
                .map(|table| procfs::sockets(&table, protocol))
                .unwrap_or_default()
        })
        .filter(|socket| socket.listening)
        .collect();
    sockets.sort_by(|first, second| (&first.protocol, first.port).cmp(&(&second.protocol, second.port)));
    sockets
}

/// Which process holds each socket: `/proc/<pid>/fd/*` point to `socket:[<inode>]`. Only root sees them all.
fn socket_owners() -> SocketOwners {
    let mut owners = SocketOwners::new();
    for pid in pids() {
        let Ok(fds) = fs::list(&format!("/proc/{pid}/fd")) else { continue };
        let Some(name) = fs::read_text(&format!("/proc/{pid}/comm")).map(|comm| comm.trim().to_string()) else {
            continue;
        };
        for inode in fds.iter().filter_map(|fd| socket_inode(pid, fd)) {
            let holders = owners.entry(inode).or_default();
            let owner = (pid, name.clone());
            if !holders.contains(&owner) {
                holders.push(owner);
            }
        }
    }
    owners
}

fn socket_inode(pid: u32, fd: &str) -> Option<u64> {
    let target = fs::read_link(&format!("/proc/{pid}/fd/{fd}"))?;
    target.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

fn port_row(socket: &NetSocket, owners: &SocketOwners) -> Value {
    let processes: Vec<Value> =
        owners.get(&socket.inode).into_iter().flatten().map(|(pid, name)| json!({"name": name, "pid": pid})).collect();
    json!({"protocol": socket.protocol, "address": socket.address, "port": socket.port, "processes": processes})
}

/// Services that should run and don't: failed units (systemd), or procd services with no running instance.
pub fn failed_services(node: &Node) -> Result<Vec<String>> {
    match init() {
        Init::Systemd => failed_units(node),
        Init::Procd => Ok(procd_services(node, None)?
            .into_iter()
            .filter(|(_, service)| service.state == ProcdState::Failed)
            .map(|(name, _)| name)
            .collect()),
        Init::None => Err(no_init()),
    }
}

fn failed_units(node: &Node) -> Result<Vec<String>> {
    let listed = node.exec_ok(&["systemctl", "list-units", "--failed", "--no-legend", "--plain", "--no-pager"])?;
    Ok(parsers::units(&listed, &node.redactor)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|unit| unit["unit"].as_str().map(String::from))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcdState {
    Running,
    Configured,
    Failed,
}

impl ProcdState {
    /// Running when an instance runs, failed when none of them does, configured when it has none.
    fn of(instances: &Map<String, Value>) -> Self {
        let running = |instance: &Value| instance.get("running").and_then(Value::as_bool) == Some(true);
        if instances.is_empty() {
            ProcdState::Configured
        } else if instances.values().any(running) {
            ProcdState::Running
        } else {
            ProcdState::Failed
        }
    }

    /// As systemd's `ActiveState` would say it.
    fn active(self) -> &'static str {
        if self == ProcdState::Failed { "failed" } else { "active" }
    }

    /// As systemd's `SubState` would say it.
    fn sub(self) -> &'static str {
        match self {
            ProcdState::Running => "running",
            ProcdState::Configured => "exited",
            ProcdState::Failed => "dead",
        }
    }

    /// Whether `services` asked for `state = [state_filter]` lists a service in this state.
    fn matches(self, state_filter: Option<&str>) -> bool {
        match state_filter {
            None | Some("all") => true,
            Some("running") => self == ProcdState::Running,
            Some("failed") => self == ProcdState::Failed,
            Some("active") => self != ProcdState::Failed,
            Some("exited") => self == ProcdState::Configured,
            _ => false,
        }
    }
}

pub struct ProcdService {
    pub state: ProcdState,
    pub instances: Map<String, Value>,
}

impl ProcdService {
    fn from_ubus(service: &Value) -> Self {
        let instances = service.get("instances").and_then(Value::as_object).cloned().unwrap_or_default();
        ProcdService { state: ProcdState::of(&instances), instances }
    }
}

/// `ubus call service list`: every procd service and its instances, or only [name]'s.
pub fn procd_services(node: &Node, name: Option<&str>) -> Result<BTreeMap<String, ProcdService>> {
    let query = match name {
        None => "{}".to_string(),
        Some(name) => json!({"name": name, "verbose": true}).to_string(),
    };
    let listed = node.exec_ok(&["ubus", "call", "service", "list", &query])?;
    // Nothing printed, or nothing readable, is no service.
    let services: Map<String, Value> = serde_json::from_str(&listed).unwrap_or_default();
    Ok(services.into_iter().map(|(name, service)| (name, ProcdService::from_ubus(&service))).collect())
}

/// Whether `/etc/rc.d` starts [name] at boot, as `service <name> enabled` says on OpenWrt.
pub fn procd_enabled(name: &str) -> bool {
    starts_at_boot(&fs::list("/etc/rc.d").unwrap_or_default(), name)
}

/// `S<digits><name>` among [links], exactly.
fn starts_at_boot(links: &[String], name: &str) -> bool {
    links.iter().any(|link| {
        link.strip_prefix('S').is_some_and(|rest| {
            let service = rest.trim_start_matches(|character: char| character.is_ascii_digit());
            service.len() < rest.len() && service == name
        })
    })
}

pub fn procd_units(node: &Node, state: Option<&str>, pattern: Option<&str>) -> Result<Value> {
    let rows = procd_services(node, None)?
        .into_iter()
        .filter(|(name, _)| pattern.is_none_or(|pattern| segment_matches(pattern, name)))
        .filter(|(_, service)| service.state.matches(state))
        .map(|(name, service)| procd_unit_row(&name, &service))
        .collect();
    Ok(Value::Array(rows))
}

/// A procd service as `services` lists a systemd unit.
fn procd_unit_row(name: &str, service: &ProcdService) -> Value {
    json!({
        "unit": name,
        "load": "loaded",
        "active": service.state.active(),
        "sub": service.state.sub(),
        "enabled": procd_enabled(name),
        "instances": service.instances.len(),
    })
}

pub fn procd_service(node: &Node, name: &str, lines: usize) -> Result<Value> {
    let service = match procd_services(node, Some(name))?.remove(name) {
        Some(service) => service,
        None if fs::exists(&format!("/etc/init.d/{name}")) => {
            ProcdService { state: ProcdState::Configured, instances: Map::new() }
        }
        None => return Err(error(ErrorCode::NotFound, format!("no service named {name}"))),
    };
    let instances: Vec<Value> = service
        .instances
        .iter()
        .filter_map(|(instance, details)| procd_instance_row(&node.redactor, instance, details))
        .collect();
    let journal =
        if lines > 0 { logread(node, lines, &LogFilter { source: Some(name), ..Default::default() })? } else { vec![] };
    Ok(json!({
        "name": name,
        "active": service.state.active(),
        "sub": service.state.sub(),
        "enabled": procd_enabled(name),
        "instances": instances,
        "journal": journal,
    }))
}

fn procd_instance_row(redactor: &Redactor, name: &str, details: &Value) -> Option<Value> {
    let fields = details.as_object()?;
    let command = fields
        .get("command")
        .and_then(Value::as_array)
        .map(|words| redactor.redact(&words.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")));
    Some(json!({
        "name": name,
        "running": fields.get("running").cloned().unwrap_or(json!(false)),
        "pid": fields.get("pid").and_then(Value::as_i64),
        "exit_code": fields.get("exit_code").and_then(Value::as_i64),
        "command": command,
    }))
}

/// What narrows a log: where a line comes from, how urgent it is, when it was written and what it says.
#[derive(Default, Clone, Copy)]
pub struct LogFilter<'a> {
    pub source: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub grep: Option<&'a str>,
}

impl LogFilter<'_> {
    fn narrows(&self) -> bool {
        self.source.is_some()
            || self.priority.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.grep.is_some()
    }

    /// Whether a line written [at] falls between `since` and `until`; one whose time is unknown doesn't.
    fn spans(&self, at: Option<i64>) -> bool {
        self.since.is_none_or(|since| at.is_some_and(|at| at >= since))
            && self.until.is_none_or(|until| at.is_some_and(|at| at <= until))
    }
}

/// The OpenWrt log (`logread`), filtered here: by source (the program, as `unit` names it), by priority, by time and
/// by grep. The last [lines] that match, oldest first.
pub fn logread(node: &Node, lines: usize, filter: &LogFilter) -> Result<Vec<Value>> {
    let window = if filter.narrows() { node.config.scan_lines } else { lines };
    let log = logread_tail(node, window)?;
    let max_level = filter.priority.and_then(parsers::priority_number);
    let rows: Vec<Value> = log
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| logread_row(&node.redactor, line, filter, max_level))
        .collect();
    Ok(last_matching_messages(rows, filter.grep, lines))
}

fn logread_tail(node: &Node, lines: usize) -> Result<String> {
    let count = lines.to_string();
    let result = node.exec(&["logread", "-l", &count], MINUTE)?;
    if result.exit_code != 0 {
        return Err(error(ErrorCode::Unavailable, format!("logread: {}", result.failure_reason())));
    }
    Ok(result.out())
}

/// [line] as a row, if [filter] lets it through. A line not in logread's shape has no fields to filter by: it goes
/// through only when nothing but grep filters.
fn logread_row(redactor: &Redactor, line: &str, filter: &LogFilter, max_level: Option<usize>) -> Option<Value> {
    let Some(entry) = procfs::logread_line(line) else {
        let unfiltered =
            filter.source.is_none() && max_level.is_none() && filter.since.is_none() && filter.until.is_none();
        return unfiltered.then(|| json!({"message": redactor.redact(line)}));
    };
    passes(&entry, filter, max_level).then(|| {
        json!({
            "time": entry.time,
            "priority": entry.priority,
            "source": entry.source,
            "pid": entry.pid,
            "message": redactor.redact(&entry.message),
        })
    })
}

/// Whether [entry] comes from the filter's source (or from `<source>-…`), is at [max_level] or more urgent, and falls
/// in its time span.
fn passes(entry: &LogreadEntry, filter: &LogFilter, max_level: Option<usize>) -> bool {
    let from_source = |source: &str| entry.source == source || entry.source.starts_with(&format!("{source}-"));
    let urgent_enough = |max: usize| parsers::priority_number(&entry.priority).is_some_and(|level| level <= max);
    filter.source.is_none_or(from_source)
        && max_level.is_none_or(urgent_enough)
        && filter.spans(time::parse_iso(&entry.time))
}

#[cfg(test)]
mod tests {
    #[test]
    fn enabled_is_an_rc_d_link_by_exact_name() {
        let links: Vec<String> =
            ["S19dnsmasq", "S19dnsmasq-extra", "K10firewall", "Sfoo"].iter().map(|link| link.to_string()).collect();
        assert!(super::starts_at_boot(&links, "dnsmasq"));
        assert!(!super::starts_at_boot(&links, "dns"));
        assert!(!super::starts_at_boot(&links, "firewall"));
        assert!(!super::starts_at_boot(&links, "foo"));
    }
}
