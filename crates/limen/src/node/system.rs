//! What the kernel says the same way on every machine —processes, sockets, mounts, memory— read from `/proc` and
//! `statvfs` directly, never through `ps`, `ss` or `df`, whose busybox versions lack the options (spec §11); and what
//! OpenWrt says of its board. What differs by init system is in `init`, `systemd` and `procd`.

use super::Node;
use crate::os::{fs, sys};
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::redactor::Redactor;
use limen_core::system::procfs::{self, Mount, NetSocket, ProcSample};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;

/// Where OpenWrt says which board it runs on.
const SYSINFO_DIR: &str = "/tmp/sysinfo";
const MAX_BOARD_BYTES: u64 = 4096;
const MAX_CMDLINE_BYTES: usize = 8192;
/// What `processes` shows of a command line.
const MAX_COMMAND_CHARS: usize = 500;

pub fn openwrt() -> bool {
    fs::exists("/etc/openwrt_release")
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
