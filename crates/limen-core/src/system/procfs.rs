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
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            Some(Account {
                name: f[0].into(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().unwrap_or(0),
                home: f[5].into(),
                shell: f[6].into(),
            })
        })
        .collect()
}

/// `/etc/group`: gid → name.
pub fn groups(text: &str) -> BTreeMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            (f.len() >= 3).then(|| f[2].parse().ok().map(|gid| (gid, f[0].to_string())))?
        })
        .collect()
}

const PSEUDO: &[&str] = &[
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
        .filter_map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() < 3 {
                return None;
            }
            let m = Mount { device: unescape(f[0]), point: unescape(f[1]), kind: f[2].into() };
            if PSEUDO.contains(&m.kind.as_str()) || (m.kind == "overlay" && m.point != "/" && m.point != "/overlay") {
                return None;
            }
            // Both sets learn from every real mount, so a device seen at a hidden mount point still hides its binds.
            let new_point = points.insert(m.point.clone());
            let new_device = m.kind == "overlay" || devices.insert(m.device.clone());
            (new_point && new_device).then_some(m)
        })
        .collect()
}

/// `\040` and the other octal escapes of `/proc/mounts`.
fn unescape(s: &str) -> String {
    static OCTAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\([0-7]{3})").unwrap());
    OCTAL
        .replace_all(s, |c: &regex::Captures| {
            char::from_u32(u32::from_str_radix(&c[1], 8).unwrap_or(0)).map(String::from).unwrap_or_default()
        })
        .into_owned()
}

/// `/proc/net/tcp`, `tcp6`, `udp` or `udp6` ([protocol] is the file name).
pub fn sockets(text: &str, protocol: &str) -> Vec<NetSocket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                return None;
            }
            let (hex_address, hex_port) = f[1].split_once(':')?;
            // TCP listens in state 0A; a bound UDP socket shows 07 (closed: no peer).
            let listening = if protocol.starts_with("tcp") { f[3] == "0A" } else { f[3] == "07" };
            Some(NetSocket {
                protocol: protocol.into(),
                address: address(hex_address)?,
                port: u16::from_str_radix(hex_port, 16).ok()?,
                listening,
                inode: f[9].parse().unwrap_or(0),
            })
        })
        .collect()
}

/// The kernel prints addresses as 32-bit words in host (little-endian) order.
pub fn address(hex: &str) -> Option<String> {
    if hex.len() % 8 != 0 || !hex.is_ascii() {
        return None;
    }
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(8)
        .map(|word| {
            let b: Vec<u8> = word
                .chunks(2)
                .map(|h| u8::from_str_radix(std::str::from_utf8(h).unwrap_or("x"), 16))
                .collect::<Result<_, _>>()
                .ok()?;
            Some(b.into_iter().rev().collect::<Vec<u8>>())
        })
        .collect::<Option<Vec<_>>>()?
        .concat();
    match bytes.len() {
        4 => Some(bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(".")),
        16 => {
            let groups: Vec<u16> = bytes.chunks(2).map(|p| u16::from(p[0]) << 8 | u16::from(p[1])).collect();
            Some(ipv6(&groups))
        }
        _ => None,
    }
}

fn ipv6(groups: &[u16]) -> String {
    // The longest run of zero groups, if longer than one, becomes `::`.
    let (mut best_start, mut best_len, mut i) = (None, 0, 0);
    while i < groups.len() {
        if groups[i] == 0 {
            let start = i;
            while i < groups.len() && groups[i] == 0 {
                i += 1;
            }
            if i - start > best_len && i - start > 1 {
                best_start = Some(start);
                best_len = i - start;
            }
        } else {
            i += 1;
        }
    }
    let hex: Vec<String> = groups.iter().map(|g| format!("{g:x}")).collect();
    match best_start {
        None => hex.join(":"),
        Some(s) => format!("{}::{}", hex[..s].join(":"), hex[s + best_len..].join(":")),
    }
}

/// `/proc/<pid>/stat`, `status` and `cmdline` of one process; None when it is unreadable.
pub fn sample(pid: u32, stat: &str, status: &str, cmdline: &str) -> Option<ProcSample> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = &stat[open + 1..close];
    // After the command: state is field 3 of the man page, index 0 here.
    let f: Vec<&str> = stat.get(close + 2..)?.split(' ').collect();
    if f.len() < 20 {
        return None;
    }
    let status_field = |key: &str| {
        status.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).map(String::from)
    };
    let command = cmdline.trim_end_matches('\0').replace('\0', " ");
    Some(ProcSample {
        pid,
        comm: comm.into(),
        uid: status_field("Uid:").and_then(|u| u.parse().ok()),
        utime_ticks: f[11].parse().unwrap_or(0),
        stime_ticks: f[12].parse().unwrap_or(0),
        start_ticks: f[19].parse().unwrap_or(0),
        rss_kb: status_field("VmRSS:").and_then(|r| r.parse().ok()).unwrap_or(0),
        cmdline: if command.is_empty() { format!("[{comm}]") } else { command },
    })
}

/// Average CPU use over the life of the process, as `ps` computes `%CPU`.
pub fn cpu_percent(p: &ProcSample, uptime_seconds: f64, ticks_per_second: u64) -> f64 {
    let alive = uptime_seconds - p.start_ticks as f64 / ticks_per_second as f64;
    if alive <= 0.0 {
        return 0.0;
    }
    let used = (p.utime_ticks + p.stime_ticks) as f64 / ticks_per_second as f64;
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
    // weekday month day time year facility.level source[pid]: message. ASCII classes: this runs on every line.
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[A-Za-z]{3} ([A-Za-z]{3}) +([0-9]{1,2}) ([0-9:]{8}) ([0-9]{4}) ([a-z0-9]+)\.([a-z]+) ([^\[:]+?)(?:\[([0-9]+)\])?: ?(.*)$")
            .unwrap()
    });
    let g = LINE.captures(line)?;
    let month = MONTHS.iter().position(|m| *m == &g[1])? + 1;
    Some(LogreadEntry {
        time: format!("{}-{month:02}-{:0>2}T{}Z", &g[4], &g[2], &g[3]),
        priority: if &g[6] == "warn" { "warning".into() } else { g[6].into() },
        source: g[7].trim().into(),
        pid: g.get(8).and_then(|p| p.as_str().parse().ok()),
        message: g[9].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_groups() {
        let a = accounts(
            "root:x:0:0:root:/root:/bin/bash\n# comment\nbroken\nlimen-read:x:998:998::/var/lib/limen:/bin/sh\n",
        );
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].name, "limen-read");
        assert_eq!(a[1].uid, 998);
        assert_eq!(groups("root:x:0:\ndocker:x:999:ana\n")[&999], "docker");
    }

    #[test]
    fn mounts_that_hold_data() {
        let text = "sysfs /sys sysfs rw 0 0\n/dev/sda1 / ext4 rw 0 0\n/dev/sda1 /var/lib/docker ext4 rw 0 0\n\
                    overlay /var/lib/docker/overlay2/x/merged overlay rw 0 0\n/dev/sdb1 /mnt/My\\040Disk ext4 rw 0 0\n\
                    overlayfs:/overlay / overlay rw 0 0\n";
        let m = mounts(text);
        assert_eq!(m.iter().map(|m| m.point.as_str()).collect::<Vec<_>>(), ["/", "/mnt/My Disk"]);
    }

    #[test]
    fn sockets_and_addresses() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0\n\
                   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 0 1 0\n";
        let s = sockets(tcp, "tcp");
        assert_eq!(s[0].address, "0.0.0.0");
        assert_eq!(s[0].port, 22);
        assert!(s[0].listening);
        assert_eq!(s[0].inode, 12345);
        assert!(!s[1].listening);
        assert_eq!(address("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(address("00000000000000000000000001000000").as_deref(), Some("::1"));
        assert_eq!(address("0000000000000000FFFF00000100007F").as_deref(), Some("::ffff:7f00:1"));
    }

    #[test]
    fn process_samples() {
        let stat = "1234 (my (odd) app) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 1000 1000000 500 18446744073709551615";
        let p =
            sample(1234, stat, "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t  2048 kB\n", "/usr/bin/app\0--flag\0")
                .unwrap();
        assert_eq!(p.comm, "my (odd) app");
        assert_eq!(p.uid, Some(1000));
        assert_eq!(p.utime_ticks, 250);
        assert_eq!(p.start_ticks, 1000);
        assert_eq!(p.rss_kb, 2048);
        assert_eq!(p.cmdline, "/usr/bin/app --flag");
        assert_eq!(cpu_percent(&p, 40.0, 100), 10.0);
        assert_eq!(sample(1, "1 (k) S 0", "", "").map(|p| p.pid), None);
    }

    #[test]
    fn logread_lines() {
        let e =
            logread_line("Sat Sep 26 17:00:00 2026 daemon.info dnsmasq[1234]: DHCPACK(br-lan) 192.168.1.20").unwrap();
        assert_eq!(e.time, "2026-09-26T17:00:00Z");
        assert_eq!(e.priority, "info");
        assert_eq!(e.source, "dnsmasq");
        assert_eq!(e.pid, Some(1234));
        assert_eq!(e.message, "DHCPACK(br-lan) 192.168.1.20");
        assert_eq!(logread_line("Sat Sep  6 07:00:00 2026 kern.warn kernel: x").unwrap().time, "2026-09-06T07:00:00Z");
        assert!(logread_line("garbage").is_none());
    }
}
