//! What differs between the machines limen runs on (spec §11): the init system and its log, and OpenWrt. What the
//! kernel says the same way everywhere —processes, sockets, mounts— is read from `/proc` directly, never through `ps`,
//! `ss` or `df`, whose busybox versions lack the options.

use super::Node;
use crate::os::{fs, sys};
use limen_core::glob::segment_matches;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::system::parsers;
use limen_core::system::procfs::{self, ProcSample};
use limen_core::time;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

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

fn no_init() -> limen_core::protocol::LimenError {
    error(ErrorCode::Unavailable, "no systemd or procd on this node")
}

/// The board, where the system says it (OpenWrt).
pub fn board() -> Option<String> {
    fs::read_text("/tmp/sysinfo/model").map(|b| b.trim().to_string()).filter(|b| !b.is_empty())
}

pub fn disks() -> Result<Value> {
    let text =
        fs::read_text("/proc/self/mounts").ok_or_else(|| error(ErrorCode::Unavailable, "no /proc/self/mounts"))?;
    let mut out = Vec::new();
    for m in procfs::mounts(&text) {
        // A file bind-mounted over another (a container's /etc/hostname) is not a filesystem to report.
        if fs::stat(&m.point).map(|i| i.kind) != Some(fs::FileType::Directory) {
            continue;
        }
        let Some((free, total)) = fs::space(&m.point) else { continue };
        if total == 0 {
            continue;
        }
        let used = total - free;
        out.push(json!({
            "mount": m.point,
            "device": m.device,
            "fstype": m.kind,
            "size_bytes": total,
            "used_bytes": used,
            "available_bytes": free,
            "used_percent": (used * 1000 / total) as f64 / 10.0,
        }));
    }
    Ok(Value::Array(out))
}

fn uptime() -> f64 {
    fs::read_text("/proc/uptime").and_then(|t| t.split(' ').next().and_then(|u| u.parse().ok())).unwrap_or(0.0)
}

fn pids() -> Vec<u32> {
    fs::list("/proc").unwrap_or_default().iter().filter_map(|p| p.parse().ok()).collect()
}

fn sample(pid: u32) -> Option<ProcSample> {
    let stat = fs::read_text(&format!("/proc/{pid}/stat"))?;
    let status = fs::read_text(&format!("/proc/{pid}/status"))?;
    let cmdline = fs::read(&format!("/proc/{pid}/cmdline"), 8192)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    procfs::sample(pid, &stat, &status, &cmdline)
}

pub fn processes(node: &Node, by_memory: bool, limit: usize) -> Value {
    let uptime = uptime();
    let mem_total_kb: u64 = fs::read_text("/proc/meminfo")
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0);
    let ticks = sys::ticks_per_second();
    let users: BTreeMap<u32, String> = fs::accounts().into_iter().map(|a| (a.uid, a.name)).collect();
    let mut samples: Vec<ProcSample> = pids().into_iter().filter_map(sample).collect();
    if by_memory {
        samples.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));
    } else {
        samples.sort_by(|a, b| procfs::cpu_percent(b, uptime, ticks).total_cmp(&procfs::cpu_percent(a, uptime, ticks)));
    }
    let rows: Vec<Value> = samples
        .iter()
        .take(limit)
        .map(|p| {
            json!({
                "pid": p.pid,
                "user": p.uid.map(|u| users.get(&u).cloned().unwrap_or_else(|| u.to_string())),
                "cpu_percent": procfs::cpu_percent(p, uptime, ticks),
                "memory_percent": (mem_total_kb > 0).then(|| (p.rss_kb * 1000 / mem_total_kb) as f64 / 10.0),
                "rss_bytes": p.rss_kb * 1024,
                "elapsed_seconds": (uptime - p.start_ticks as f64 / ticks as f64).max(0.0) as i64,
                "command": node.redactor.redact(&p.cmdline).chars().take(500).collect::<String>(),
            })
        })
        .collect();
    Value::Array(rows)
}

pub fn ports() -> Value {
    let mut sockets: Vec<procfs::NetSocket> = ["tcp", "tcp6", "udp", "udp6"]
        .iter()
        .flat_map(|proto| {
            fs::read_text(&format!("/proc/net/{proto}")).map(|t| procfs::sockets(&t, proto)).unwrap_or_default()
        })
        .filter(|s| s.listening)
        .collect();
    sockets.sort_by(|a, b| (&a.protocol, a.port).cmp(&(&b.protocol, b.port)));
    // Which process holds each socket: /proc/<pid>/fd/* point to `socket:[<inode>]`. Only root sees them all.
    let mut owners: BTreeMap<u64, Vec<(u32, String)>> = BTreeMap::new();
    for pid in pids() {
        let Ok(fds) = fs::list(&format!("/proc/{pid}/fd")) else { continue };
        let Some(comm) = fs::read_text(&format!("/proc/{pid}/comm")).map(|c| c.trim().to_string()) else { continue };
        for fd in fds {
            let Some(target) = fs::read_link(&format!("/proc/{pid}/fd/{fd}")) else { continue };
            let Some(inode) =
                target.strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')).and_then(|i| i.parse().ok())
            else {
                continue;
            };
            let list = owners.entry(inode).or_default();
            if !list.contains(&(pid, comm.clone())) {
                list.push((pid, comm.clone()));
            }
        }
    }
    let rows: Vec<Value> = sockets
        .iter()
        .map(|s| {
            let processes: Vec<Value> = owners
                .get(&s.inode)
                .into_iter()
                .flatten()
                .map(|(pid, comm)| json!({"name": comm, "pid": pid}))
                .collect();
            json!({"protocol": s.protocol, "address": s.address, "port": s.port, "processes": processes})
        })
        .collect();
    Value::Array(rows)
}

/// Services that should run and don't: failed units (systemd), or procd services with no running instance.
pub fn failed_services(node: &Node) -> Result<Vec<String>> {
    match init() {
        Init::Systemd => {
            let out = node.exec_ok(&["systemctl", "list-units", "--failed", "--no-legend", "--plain", "--no-pager"])?;
            Ok(parsers::units(&out)
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|u| u["unit"].as_str().map(String::from))
                .collect())
        }
        Init::Procd => Ok(procd_services(node, None)?
            .into_iter()
            .filter(|(_, s)| s.state == ProcdState::Failed)
            .map(|(n, _)| n)
            .collect()),
        Init::None => Err(no_init()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcdState {
    Running,
    Configured,
    Failed,
}

impl ProcdState {
    fn active(self) -> &'static str {
        if self == ProcdState::Failed { "failed" } else { "active" }
    }

    fn sub(self) -> &'static str {
        match self {
            ProcdState::Running => "running",
            ProcdState::Configured => "exited",
            ProcdState::Failed => "dead",
        }
    }
}

pub struct ProcdService {
    pub state: ProcdState,
    pub instances: Map<String, Value>,
}

/// `ubus call service list`: every procd service and its instances.
pub fn procd_services(node: &Node, name: Option<&str>) -> Result<BTreeMap<String, ProcdService>> {
    let args = match name {
        None => "{}".to_string(),
        Some(n) => json!({"name": n, "verbose": true}).to_string(),
    };
    let out = node.exec_ok(&["ubus", "call", "service", "list", &args])?;
    let all: Map<String, Value> =
        serde_json::from_str(if out.trim().is_empty() { "{}" } else { &out }).unwrap_or_default();
    Ok(all
        .into_iter()
        .map(|(name, v)| {
            let instances = v.get("instances").and_then(Value::as_object).cloned().unwrap_or_default();
            let running: Vec<bool> =
                instances.values().map(|i| i.get("running").and_then(Value::as_bool) == Some(true)).collect();
            let state = if running.is_empty() {
                ProcdState::Configured
            } else if running.iter().any(|r| *r) {
                ProcdState::Running
            } else {
                ProcdState::Failed
            };
            (name, ProcdService { state, instances })
        })
        .collect())
}

/// Whether `/etc/rc.d` starts [name] at boot, as `service <name> enabled` says on OpenWrt.
pub fn procd_enabled(name: &str) -> bool {
    starts_at_boot(&fs::list("/etc/rc.d").unwrap_or_default(), name)
}

/// `S<digits><name>` among [links], exactly.
fn starts_at_boot(links: &[String], name: &str) -> bool {
    links.iter().any(|link| {
        link.strip_prefix('S').is_some_and(|rest| {
            let service = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            service.len() < rest.len() && service == name
        })
    })
}

pub fn procd_units(node: &Node, state: Option<&str>, pattern: Option<&str>) -> Result<Value> {
    let rows: Vec<Value> = procd_services(node, None)?
        .into_iter()
        .filter(|(name, _)| pattern.is_none_or(|p| segment_matches(p, name)))
        .filter(|(_, s)| match state {
            None | Some("all") => true,
            Some("running") => s.state == ProcdState::Running,
            Some("failed") => s.state == ProcdState::Failed,
            Some("active") => s.state != ProcdState::Failed,
            Some("exited") => s.state == ProcdState::Configured,
            _ => false,
        })
        .map(|(name, s)| {
            json!({
                "unit": name,
                "load": "loaded",
                "active": s.state.active(),
                "sub": s.state.sub(),
                "enabled": procd_enabled(&name),
                "instances": s.instances.len(),
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

pub fn procd_service(node: &Node, name: &str, lines: usize) -> Result<Value> {
    let s = match procd_services(node, Some(name))?.remove(name) {
        Some(s) => s,
        None if fs::exists(&format!("/etc/init.d/{name}")) => {
            ProcdService { state: ProcdState::Configured, instances: Map::new() }
        }
        None => return Err(error(ErrorCode::NotFound, format!("no service named {name}"))),
    };
    let instances: Vec<Value> = s
        .instances
        .iter()
        .filter_map(|(instance, v)| {
            let o = v.as_object()?;
            let command = o
                .get("command")
                .and_then(Value::as_array)
                .map(|c| node.redactor.redact(&c.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")));
            Some(json!({
                "name": instance,
                "running": o.get("running").cloned().unwrap_or(json!(false)),
                "pid": o.get("pid").and_then(Value::as_i64),
                "exit_code": o.get("exit_code").and_then(Value::as_i64),
                "command": command,
            }))
        })
        .collect();
    let journal =
        if lines > 0 { logread(node, lines, &LogFilter { source: Some(name), ..Default::default() })? } else { vec![] };
    Ok(json!({
        "name": name,
        "active": s.state.active(),
        "sub": s.state.sub(),
        "enabled": procd_enabled(name),
        "instances": instances,
        "journal": journal,
    }))
}

#[derive(Default)]
pub struct LogFilter<'a> {
    pub source: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub grep: Option<&'a str>,
}

/// The OpenWrt log (`logread`), filtered here: by source (the program, as `unit` names it), by priority, by time and
/// by grep. The last [lines] that match, oldest first.
pub fn logread(node: &Node, lines: usize, f: &LogFilter) -> Result<Vec<Value>> {
    let filtered =
        f.source.is_some() || f.priority.is_some() || f.since.is_some() || f.until.is_some() || f.grep.is_some();
    let count = if filtered { node.config.scan_lines } else { lines }.to_string();
    let r = node.exec(&["logread", "-l", &count], Duration::from_secs(60))?;
    if r.exit_code != 0 {
        let err = r.err();
        let last = err.trim().lines().last().map(String::from).unwrap_or_else(|| format!("exit {}", r.exit_code));
        return Err(error(ErrorCode::Unavailable, format!("logread: {last}")));
    }
    let max_level = f.priority.and_then(parsers::priority_number);
    let grep = f.grep.map(str::to_lowercase);
    let has = |text: &str| grep.as_ref().is_none_or(|g| text.to_lowercase().contains(g));
    let out = r.out();
    let entries: Vec<Value> = out
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let Some(e) = procfs::logread_line(line) else {
                let unfiltered = f.source.is_none() && max_level.is_none() && f.since.is_none() && f.until.is_none();
                return (unfiltered && has(line)).then(|| json!({"message": node.redactor.redact(line)}));
            };
            if let Some(source) = f.source {
                if e.source != source && !e.source.starts_with(&format!("{source}-")) {
                    return None;
                }
            }
            if let Some(max) = max_level {
                if parsers::priority_number(&e.priority).is_none_or(|p| p > max) {
                    return None;
                }
            }
            let at = time::parse_iso(&e.time);
            if f.since.is_some_and(|s| at.is_none_or(|t| t < s)) || f.until.is_some_and(|u| at.is_none_or(|t| t > u)) {
                return None;
            }
            if !has(&e.message) {
                return None;
            }
            Some(json!({
                "time": e.time,
                "priority": e.priority,
                "source": e.source,
                "pid": e.pid,
                "message": node.redactor.redact(&e.message),
            }))
        })
        .collect();
    let from = entries.len().saturating_sub(lines);
    Ok(entries[from..].to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn enabled_is_an_rc_d_link_by_exact_name() {
        let links: Vec<String> =
            ["S19dnsmasq", "S19dnsmasq-extra", "K10firewall", "Sfoo"].iter().map(|s| s.to_string()).collect();
        assert!(super::starts_at_boot(&links, "dnsmasq"));
        assert!(!super::starts_at_boot(&links, "dns"));
        assert!(!super::starts_at_boot(&links, "firewall"));
        assert!(!super::starts_at_boot(&links, "foo"));
    }
}
