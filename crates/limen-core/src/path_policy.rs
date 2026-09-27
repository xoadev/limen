//! What `read_file`, `list_dir` and file logs may open (spec §7.1). Always given the **resolved** path —symlinks and
//! `..` gone— because matching the text the client sent would let a link walk out of the allowlist.

use crate::glob::Glob;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Denied(String),
}

#[derive(Debug, Clone)]
pub struct PathPolicy {
    allow: Vec<Glob>,
    deny: Vec<Glob>,
    /// limen's own files wherever the configuration puts them —the audit log, the runs' output, the repository
    /// token—, and what is below them: paths, not patterns.
    private: Vec<String>,
}

impl PathPolicy {
    /// The patterns were checked when the configuration was read; one that isn't absolute matches nothing.
    pub fn new(allow: &[String], deny: &[String]) -> Self {
        let globs = |list: &[String]| list.iter().filter_map(|p| Glob::new(p).ok()).collect();
        Self { allow: globs(allow), deny: globs(deny), private: vec![] }
    }

    pub fn with_private(mut self, paths: Vec<String>) -> Self {
        self.private = paths.into_iter().map(|p| normalize(&p)).collect();
        self
    }

    fn is_private(&self, resolved: &str) -> bool {
        self.private
            .iter()
            .any(|p| resolved == p || resolved.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('/')))
    }

    pub fn check(&self, resolved: &str) -> Decision {
        if built_in_deny().iter().any(|g| g.matches(resolved)) || self.is_private(resolved) {
            return Decision::Denied(format!("{resolved} is never readable"));
        }
        if let Some(g) = self.deny.iter().find(|g| g.matches(resolved)) {
            return Decision::Denied(format!("{resolved} is denied by files.deny ({g})"));
        }
        if !self.allow.iter().any(|g| g.matches(resolved)) {
            return Decision::Denied(format!("{resolved} is not in files.allow"));
        }
        Decision::Allowed
    }

    pub fn allowed(&self, resolved: &str) -> bool {
        self.check(resolved) == Decision::Allowed
    }

    /// A directory that is not allowed itself but on the way to something that is: listable, to find it.
    pub fn leads_to(&self, dir: &str) -> bool {
        !built_in_deny().iter().any(|g| g.matches(dir))
            && !self.is_private(dir)
            && !self.deny.iter().any(|g| g.matches(dir))
            && self.allow.iter().any(|g| g.may_match_below(dir))
    }
}

/// An absolute path with `.` and `..` resolved as text, for a path that doesn't exist and so has no realpath.
pub fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// Can't be overridden by any configuration (spec §7.1): account and sudo secrets and their backups, private keys,
/// VPN and Wi-Fi credentials, systemd's service credentials, limen's own directories, root's home, and the
/// pseudo-filesystems, where a "file" can be a process's environment or a whole disk.
pub fn built_in_deny() -> &'static [Glob] {
    static DENY: OnceLock<Vec<Glob>> = OnceLock::new();
    DENY.get_or_init(|| {
        [
            "/etc/shadow",
            "/etc/shadow-",
            "/etc/gshadow",
            "/etc/gshadow-",
            "/etc/sudoers*",
            "/etc/sudoers.d/**",
            "/etc/ssh/ssh_host_*_key",
            "/etc/dropbear/dropbear_*_host_key",
            "**/.ssh/id_*",
            "/etc/ssl/private/**",
            "/etc/wireguard/**",
            "/etc/NetworkManager/system-connections/**",
            "/etc/config/wireless",
            "/run/credentials/**",
            "/var/backups/*shadow*",
            "/etc/limen/**",
            "/var/log/limen/**",
            "/root/**",
            "/proc/**",
            "/sys/**",
            "/dev/**",
        ]
        .iter()
        .map(|p| Glob::new(p).unwrap())
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], deny: &[&str]) -> PathPolicy {
        let s = |l: &[&str]| l.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        PathPolicy::new(&s(allow), &s(deny))
    }

    #[test]
    fn allow_deny_and_built_in() {
        let p = policy(&["/etc/**", "/opt/**"], &["**/*.env"]);
        assert_eq!(p.check("/etc/nginx/nginx.conf"), Decision::Allowed);
        assert!(!p.allowed("/opt/app/.env"));
        assert!(!p.allowed("/srv/data"));
        for secret in [
            "/etc/shadow",
            "/etc/sudoers",
            "/etc/sudoers.tmp",
            "/etc/sudoers.d/limen",
            "/etc/dropbear/dropbear_ed25519_host_key",
            "/etc/ssh/ssh_host_rsa_key",
            "/home/ana/.ssh/id_ed25519",
            "/etc/ssl/private/key.pem",
            "/etc/wireguard/wg0.conf",
            "/etc/NetworkManager/system-connections/home.nmconnection",
            "/etc/limen/limen.toml",
            "/etc/config/wireless",
            "/run/credentials/app.service/db",
            "/var/backups/shadow.bak",
            "/var/log/limen/audit.jsonl",
        ] {
            assert!(!p.allowed(secret), "{secret}");
        }
        let everything = policy(&["/**"], &[]);
        assert!(!everything.allowed("/proc/1/environ"));
        assert!(!everything.allowed("/dev/sda"));
        assert!(!everything.allowed("/root/.bash_history"));
        assert!(everything.allowed("/etc/dropbear/dropbear_ed25519_host_key.pub"));
    }

    #[test]
    fn limens_own_files_wherever_they_are() {
        let p =
            policy(&["/srv/**"], &[]).with_private(vec!["/srv/limen/audit.jsonl".into(), "/srv/limen/runs/".into()]);
        assert!(!p.allowed("/srv/limen/audit.jsonl"));
        assert!(!p.allowed("/srv/limen/runs/x.log"));
        assert!(!p.leads_to("/srv/limen/runs"));
        assert!(p.allowed("/srv/limen/audit.jsonl.old") && p.allowed("/srv/limen/runs2"));
    }

    #[test]
    fn empty_allowlist_allows_nothing() {
        let p = policy(&[], &[]);
        assert!(!p.allowed("/etc/hostname"));
        assert!(!p.leads_to("/etc"));
    }

    #[test]
    fn leads_to_stops_at_denied_directories() {
        let p = policy(&["/etc/**"], &[]);
        assert!(p.leads_to("/"));
        assert!(!p.leads_to("/etc/ssl/private"));
    }

    #[test]
    fn normalize_resolves_dots_as_text() {
        assert_eq!(normalize("/etc/nginx/../shadow"), "/etc/shadow");
        assert_eq!(normalize("/../../etc/./shadow"), "/etc/shadow");
        assert_eq!(normalize("/a/.."), "/");
        assert_eq!(normalize("//a//b/"), "/a/b");
    }
}
