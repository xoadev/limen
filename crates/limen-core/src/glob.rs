//! A path pattern of `files.allow` and `files.deny` (spec §7.1). Absolute; `**` is any number of segments, zero
//! included, so a trailing `**` also matches the directory itself; `*` is any run of characters within a segment and
//! `?` one character. Nothing else is special.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    pattern: String,
    segments: Vec<String>,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Self, String> {
        if !pattern.starts_with('/') && !pattern.starts_with("**") {
            return Err(format!("'{pattern}' is not an absolute pattern"));
        }
        Ok(Self { pattern: pattern.into(), segments: split(pattern) })
    }

    pub fn matches(&self, path: &str) -> bool {
        matches(&self.segments, &split(path), false)
    }

    /// Whether something at or below directory [dir] could match: what lets `list_dir` walk towards allowed files.
    pub fn may_match_below(&self, dir: &str) -> bool {
        matches(&self.segments, &split(dir), true)
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pattern)
    }
}

fn split(path: &str) -> Vec<String> {
    path.split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn matches(pat: &[String], path: &[String], prefix_only: bool) -> bool {
    if path.is_empty() && prefix_only {
        return true;
    }
    let Some((first, rest)) = pat.split_first() else {
        return path.is_empty();
    };
    if first == "**" {
        return (0..=path.len()).any(|skip| matches(rest, &path[skip..], prefix_only));
    }
    match path.split_first() {
        Some((segment, remaining)) => segment_matches(first, segment) && matches(rest, remaining, prefix_only),
        None => false,
    }
}

/// `*` and `?` within one segment, by backtracking over the last `*`.
pub fn segment_matches(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str) -> Glob {
        Glob::new(p).unwrap()
    }

    #[test]
    fn segments() {
        assert!(g("/etc/nginx/**").matches("/etc/nginx/sites/default"));
        assert!(g("/etc/nginx/**").matches("/etc/nginx"));
        assert!(!g("/etc/nginx/**").matches("/etc/nginx2/x"));
        assert!(g("/opt/stacks/*/compose.yaml").matches("/opt/stacks/immich/compose.yaml"));
        assert!(!g("/opt/stacks/*/compose.yaml").matches("/opt/stacks/a/b/compose.yaml"));
        assert!(g("/var/log/*.log").matches("/var/log/syslog.log"));
        assert!(!g("/var/log/*.log").matches("/var/log/syslog"));
        assert!(g("**/*.env").matches("/opt/app/.env"));
        assert!(g("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!g("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key.pub"));
        assert!(g("/a/?.txt").matches("/a/b.txt"));
        assert!(Glob::new("etc/*").is_err());
    }

    #[test]
    fn may_match_below_walks_towards_allowed_files() {
        let glob = g("/opt/stacks/*/compose.yaml");
        assert!(glob.may_match_below("/opt"));
        assert!(glob.may_match_below("/opt/stacks/immich"));
        assert!(!glob.may_match_below("/srv"));
        assert!(g("/var/**/x").may_match_below("/var/lib/a/b"));
    }
}
