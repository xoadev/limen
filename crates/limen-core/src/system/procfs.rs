//! What the kernel says in `/proc` and `/etc`, parsed without the programs that usually read it (`ps`, `ss`, `df`):
//! they differ between distributions and busybox.

use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub point: String,
    pub kind: String,
}

/// One socket of `/proc/net/{tcp,udp}{,6}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSocket {
    pub protocol: String,
    pub address: String,
    pub port: u16,
    pub listening: bool,
    pub inode: u64,
}

/// The fields of `/proc/<pid>/stat` and `status` that `processes` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSample {
    pub pid: u32,
    pub comm: String,
    pub uid: Option<u32>,
    pub utime_ticks: u64,
    pub stime_ticks: u64,
    pub start_ticks: u64,
    pub rss_kb: u64,
    pub cmdline: String,
}

/// `/etc/passwd`.
pub fn accounts(text: &str) -> Vec<Account> {
    text.lines().filter(|line| !line.starts_with('#')).filter_map(account).collect()
}

fn account(line: &str) -> Option<Account> {
    let fields: Vec<&str> = line.split(':').collect();
    let [name, _password, uid, gid, _gecos, home, shell, ..] = fields[..] else {
        return None;
    };
    Some(Account {
        name: name.into(),
        uid: uid.parse().ok()?,
        gid: gid.parse().unwrap_or(0),
        home: home.into(),
        shell: shell.into(),
    })
}

/// `/etc/group`: gid → name.
pub fn groups(text: &str) -> BTreeMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _password, gid, ..] = fields[..] else {
                return None;
            };
            Some((gid.parse().ok()?, name.to_string()))
        })
        .collect()
}

const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "bpf",
    "debugfs",
    "tracefs",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "autofs",
    "efivarfs",
    "binfmt_misc",
    "nsfs",
    "rpc_pipefs",
    "ramfs",
    "squashfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "nfsd",
];

/// The mounts of `/proc/mounts` that hold data: no pseudo-filesystems, no container layers (an `overlay` only at `/`
/// or `/overlay`, which is where OpenWrt keeps its writable root), each device once.
pub fn mounts(text: &str) -> Vec<Mount> {
    let mut points = HashSet::new();
    let mut devices = HashSet::new();
    text.lines()
        .filter_map(mount)
        .filter(holds_data)
        .filter(|mount| {
            // Both sets learn from every real mount, so a device seen at a hidden mount point still hides its binds.
            let new_point = points.insert(mount.point.clone());
            let new_device = mount.kind == "overlay" || devices.insert(mount.device.clone());
            new_point && new_device
        })
        .collect()
}

fn mount(line: &str) -> Option<Mount> {
    let fields: Vec<&str> = line.split(' ').collect();
    let [device, point, kind, ..] = fields[..] else {
        return None;
    };
    Some(Mount { device: unescape(device), point: unescape(point), kind: kind.into() })
}

fn holds_data(mount: &Mount) -> bool {
    let container_layer = mount.kind == "overlay" && mount.point != "/" && mount.point != "/overlay";
    !PSEUDO_FILESYSTEMS.contains(&mount.kind.as_str()) && !container_layer
}

/// `\040` and the other octal escapes of `/proc/mounts`.
fn unescape(text: &str) -> String {
    static OCTAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\([0-7]{3})").expect("the octal escape pattern is a valid regex"));
    OCTAL
        .replace_all(text, |escape: &regex::Captures| {
            char::from_u32(u32::from_str_radix(&escape[1], 8).unwrap_or(0)).map(String::from).unwrap_or_default()
        })
        .into_owned()
}

/// `/proc/net/tcp`, `tcp6`, `udp` or `udp6` ([protocol] is the file name).
pub fn sockets(text: &str, protocol: &str) -> Vec<NetSocket> {
    text.lines().skip(1).filter_map(|line| socket(line, protocol)).collect()
}

fn socket(line: &str, protocol: &str) -> Option<NetSocket> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [_slot, local, _remote, state, _queues, _timer, _retransmits, _uid, _timeout, inode, ..] = fields[..] else {
        return None;
    };
    let (hex_address, hex_port) = local.split_once(':')?;
    // TCP listens in state 0A; a bound UDP socket shows 07 (closed: no peer).
    let listening = if protocol.starts_with("tcp") { state == "0A" } else { state == "07" };
    Some(NetSocket {
        protocol: protocol.into(),
        address: address(hex_address)?,
        port: u16::from_str_radix(hex_port, 16).ok()?,
        listening,
        inode: inode.parse().unwrap_or(0),
    })
}

/// The kernel prints addresses as 32-bit words in host (little-endian) order.
pub fn address(hex: &str) -> Option<String> {
    if hex.len() % 8 != 0 || !hex.is_ascii() {
        return None;
    }
    let bytes: Vec<u8> = hex.as_bytes().chunks(8).map(word_bytes).collect::<Option<Vec<_>>>()?.concat();
    match bytes.len() {
        4 => Some(bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(".")),
        16 => {
            let groups: Vec<u16> = bytes.chunks(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
            Some(ipv6(&groups))
        }
        _ => None,
    }
}

/// One word's eight hex digits, in host order, as its four bytes in network order.
fn word_bytes(word: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = word
        .chunks(2)
        .map(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    bytes.reverse();
    Some(bytes)
}

fn ipv6(groups: &[u16]) -> String {
    let hex: Vec<String> = groups.iter().map(|group| format!("{group:x}")).collect();
    match longest_zero_run(groups) {
        None => hex.join(":"),
        Some((start, length)) => format!("{}::{}", hex[..start].join(":"), hex[start + length..].join(":")),
    }
}

/// Where the longest run of zero groups starts and how long it is, if longer than one: what becomes `::`. The first
/// of two equal runs wins.
fn longest_zero_run(groups: &[u16]) -> Option<(usize, usize)> {
    let mut longest = None;
    let mut longest_length = 1;
    let mut start = 0;
    while start < groups.len() {
        let length = groups[start..].iter().take_while(|group| **group == 0).count();
        if length > longest_length {
            longest = Some((start, length));
            longest_length = length;
        }
        start += length.max(1);
    }
    longest
}

// The fields of `/proc/<pid>/stat` after the command, which is field 2 of `man proc_pid_stat`: field N is index N - 3.
const UTIME: usize = 11;
const STIME: usize = 12;
const STARTTIME: usize = 19;

/// `/proc/<pid>/stat`, `status` and `cmdline` of one process; None when it is unreadable.
pub fn sample(pid: u32, stat: &str, status: &str, cmdline: &str) -> Option<ProcSample> {
    // The command is between the first `(` and the last `)`: it can have both.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = &stat[open + 1..close];
    let fields: Vec<&str> = stat.get(close + 2..)?.split(' ').collect();
    if fields.len() <= STARTTIME {
        return None;
    }
    let command = cmdline.trim_end_matches('\0').replace('\0', " ");
    Some(ProcSample {
        pid,
        comm: comm.into(),
        uid: status_value(status, "Uid:").and_then(|uid| uid.parse().ok()),
        utime_ticks: fields[UTIME].parse().unwrap_or(0),
        stime_ticks: fields[STIME].parse().unwrap_or(0),
        start_ticks: fields[STARTTIME].parse().unwrap_or(0),
        rss_kb: status_value(status, "VmRSS:").and_then(|rss| rss.parse().ok()).unwrap_or(0),
        cmdline: if command.is_empty() { format!("[{comm}]") } else { command },
    })
}

/// The first value of a `/proc/<pid>/status` line: `Uid:\t1000\t1000…` gives `1000`.
fn status_value<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status.lines().find(|line| line.starts_with(key)).and_then(|line| line.split_whitespace().nth(1))
}

/// Average CPU use over the life of the process, as `ps` computes `%CPU`.
pub fn cpu_percent(process: &ProcSample, uptime_seconds: f64, ticks_per_second: u64) -> f64 {
    let alive = uptime_seconds - process.start_ticks as f64 / ticks_per_second as f64;
    if alive <= 0.0 {
        return 0.0;
    }
    let used = (process.utime_ticks + process.stime_ticks) as f64 / ticks_per_second as f64;
    (used / alive * 1000.0).trunc() / 10.0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogreadEntry {
    pub time: String,
    pub priority: String,
    pub source: String,
    pub pid: Option<i64>,
    pub message: String,
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// One `logread` line (OpenWrt): `Sat Sep 26 17:00:00 2026 daemon.info dnsmasq[1234]: message`. The time is the one
/// logread prints, UTC when it runs with `TZ=UTC`.
pub fn logread_line(line: &str) -> Option<LogreadEntry> {
    // ASCII classes: this runs on every line.
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"^[A-Za-z]{3} (?<month>[A-Za-z]{3}) +(?<day>[0-9]{1,2}) (?<time>[0-9:]{8}) (?<year>[0-9]{4}) ",
            r"[a-z0-9]+\.(?<level>[a-z]+) (?<source>[^\[:]+?)(?:\[(?<pid>[0-9]+)\])?: ?(?<message>.*)$",
        ))
        .expect("the logread line pattern is a valid regex")
    });
    let fields = LINE.captures(line)?;
    let month = MONTHS.iter().position(|name| *name == &fields["month"])? + 1;
    Some(LogreadEntry {
        time: format!("{}-{month:02}-{:0>2}T{}Z", &fields["year"], &fields["day"], &fields["time"]),
        priority: if &fields["level"] == "warn" { "warning".into() } else { fields["level"].into() },
        source: fields["source"].trim().into(),
        pid: fields.name("pid").and_then(|pid| pid.as_str().parse().ok()),
        message: fields["message"].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_groups() {
        let users = accounts(
            "root:x:0:0:root:/root:/bin/bash\n# comment\nbroken\nlimen-read:x:998:998::/var/lib/limen:/bin/sh\n",
        );
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].name, "limen-read");
        assert_eq!(users[1].uid, 998);
        assert_eq!(groups("root:x:0:\ndocker:x:999:ana\n")[&999], "docker");
    }

    #[test]
    fn mounts_that_hold_data() {
        let text = "sysfs /sys sysfs rw 0 0\n/dev/sda1 / ext4 rw 0 0\n/dev/sda1 /var/lib/docker ext4 rw 0 0\n\
                    overlay /var/lib/docker/overlay2/x/merged overlay rw 0 0\n/dev/sdb1 /mnt/My\\040Disk ext4 rw 0 0\n\
                    overlayfs:/overlay / overlay rw 0 0\n";
        let points: Vec<String> = mounts(text).into_iter().map(|mount| mount.point).collect();
        assert_eq!(points, ["/", "/mnt/My Disk"]);
    }

    #[test]
    fn sockets_and_addresses() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0\n\
                   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 0 1 0\n";
        let parsed = sockets(tcp, "tcp");
        assert_eq!(parsed[0].address, "0.0.0.0");
        assert_eq!(parsed[0].port, 22);
        assert!(parsed[0].listening);
        assert_eq!(parsed[0].inode, 12345);
        assert!(!parsed[1].listening);
        assert_eq!(address("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(address("00000000000000000000000001000000").as_deref(), Some("::1"));
        assert_eq!(address("0000000000000000FFFF00000100007F").as_deref(), Some("::ffff:7f00:1"));
    }

    #[test]
    fn process_samples() {
        let stat = "1234 (my (odd) app) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 1000 1000000 500 18446744073709551615";
        let process =
            sample(1234, stat, "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t  2048 kB\n", "/usr/bin/app\0--flag\0")
                .unwrap();
        assert_eq!(process.comm, "my (odd) app");
        assert_eq!(process.uid, Some(1000));
        assert_eq!(process.utime_ticks, 250);
        assert_eq!(process.start_ticks, 1000);
        assert_eq!(process.rss_kb, 2048);
        assert_eq!(process.cmdline, "/usr/bin/app --flag");
        assert_eq!(cpu_percent(&process, 40.0, 100), 10.0);
        assert_eq!(sample(1, "1 (k) S 0", "", "").map(|process| process.pid), None);
    }

    #[test]
    fn logread_lines() {
        let entry =
            logread_line("Sat Sep 26 17:00:00 2026 daemon.info dnsmasq[1234]: DHCPACK(br-lan) 192.168.1.20").unwrap();
        assert_eq!(entry.time, "2026-09-26T17:00:00Z");
        assert_eq!(entry.priority, "info");
        assert_eq!(entry.source, "dnsmasq");
        assert_eq!(entry.pid, Some(1234));
        assert_eq!(entry.message, "DHCPACK(br-lan) 192.168.1.20");
        assert_eq!(logread_line("Sat Sep  6 07:00:00 2026 kern.warn kernel: x").unwrap().time, "2026-09-06T07:00:00Z");
        assert!(logread_line("garbage").is_none());
    }

    #[test]
    fn a_long_log_is_parsed_in_a_moment() {
        // `logs` with a filter scans 100,000 lines: a pattern built per line made that minutes.
        let line = "Sat Sep 26 17:00:00 2026 daemon.info dnsmasq[1234]: DHCPACK(br-lan) 192.168.1.20";
        let start = std::time::Instant::now();
        assert_eq!((0..20_000).filter_map(|_| logread_line(line)).count(), 20_000);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
    }
}
