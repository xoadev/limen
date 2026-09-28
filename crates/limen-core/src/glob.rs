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
        Ok(Self { pattern: pattern.into(), segments: segments_of(pattern) })
    }

    pub fn matches(&self, path: &str) -> bool {
        segments_match(&self.segments, &segments_of(path), false)
    }

    /// Whether something at or below directory [dir] could match: what lets `list_dir` walk towards allowed files.
    pub fn may_match_below(&self, dir: &str) -> bool {
        segments_match(&self.segments, &segments_of(dir), true)
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.pattern)
    }
}

fn segments_of(path: &str) -> Vec<String> {
    path.split('/').filter(|segment| !segment.is_empty()).map(String::from).collect()
}

/// With [prefix_only], whether [path] could be the start of a match rather than a whole one.
fn segments_match(pattern: &[String], path: &[String], prefix_only: bool) -> bool {
    if path.is_empty() && prefix_only {
        return true;
    }
    let Some((first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if first == "**" {
        return (0..=path.len()).any(|skipped| segments_match(rest, &path[skipped..], prefix_only));
    }
    match path.split_first() {
        Some((segment, remaining)) => segment_matches(first, segment) && segments_match(rest, remaining, prefix_only),
        None => false,
    }
}

/// `*` and `?` within one segment, by backtracking over the last `*`.
pub fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pattern_at, mut text_at) = (0, 0);
    // The last `*` seen and where the text it swallows ends: on a mismatch, it swallows one character more.
    let (mut last_star, mut star_end) = (None, 0);
    while text_at < text.len() {
        let wanted = pattern.get(pattern_at);
        if wanted.is_some_and(|&symbol| symbol == '?' || symbol == text[text_at]) {
            pattern_at += 1;
            text_at += 1;
        } else if wanted == Some(&'*') {
            last_star = Some(pattern_at);
            pattern_at += 1;
            star_end = text_at;
        } else if let Some(star) = last_star {
            pattern_at = star + 1;
            star_end += 1;
            text_at = star_end;
        } else {
            return false;
        }
    }
    pattern[pattern_at..].iter().all(|&unmatched| unmatched == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(pattern: &str) -> Glob {
        Glob::new(pattern).unwrap()
    }

    #[test]
    fn segments() {
        assert!(glob("/etc/nginx/**").matches("/etc/nginx/sites/default"));
        assert!(glob("/etc/nginx/**").matches("/etc/nginx"));
        assert!(!glob("/etc/nginx/**").matches("/etc/nginx2/x"));
        assert!(glob("/opt/stacks/*/compose.yaml").matches("/opt/stacks/immich/compose.yaml"));
        assert!(!glob("/opt/stacks/*/compose.yaml").matches("/opt/stacks/a/b/compose.yaml"));
        assert!(glob("/var/log/*.log").matches("/var/log/syslog.log"));
        assert!(!glob("/var/log/*.log").matches("/var/log/syslog"));
        assert!(glob("**/*.env").matches("/opt/app/.env"));
        assert!(glob("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!glob("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key.pub"));
        assert!(glob("/a/?.txt").matches("/a/b.txt"));
        assert!(Glob::new("etc/*").is_err());
    }

    #[test]
    fn may_match_below_walks_towards_allowed_files() {
        let compose_files = glob("/opt/stacks/*/compose.yaml");
        assert!(compose_files.may_match_below("/opt"));
        assert!(compose_files.may_match_below("/opt/stacks/immich"));
        assert!(!compose_files.may_match_below("/srv"));
        assert!(glob("/var/**/x").may_match_below("/var/lib/a/b"));
    }
}
