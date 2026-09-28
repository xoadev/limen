//! `/etc/limen/limen.toml` (spec §7.1). Every key has a default; a missing file means nothing is readable and no
//! script is offered.

use super::{ConfigResult, absolute, fail, positive};
use crate::glob::Glob;
use crate::own_regex;
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

pub const PATH: &str = "/etc/limen/limen.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub max_file_bytes: usize,
    /// The directories whose scripts this node offers.
    pub packs: Vec<String>,
    pub redact_names: Vec<String>,
    pub redact_patterns: Vec<String>,
    pub max_lines: usize,
    pub scan_lines: usize,
    pub max_response_bytes: usize,
    /// Requests that may run at once on this node, from every hub together.
    pub concurrency: usize,
    pub audit: String,
    pub audit_max_bytes: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            allow: vec![],
            deny: vec![],
            max_file_bytes: 256 * 1024,
            packs: vec![],
            redact_names: vec![],
            redact_patterns: vec![],
            max_lines: 2000,
            scan_lines: 100_000,
            max_response_bytes: 1024 * 1024,
            concurrency: 8,
            audit: "/var/log/limen/audit.jsonl".into(),
            audit_max_bytes: 5 * 1024 * 1024,
        }
    }
}

/// The file as written; [NodeConfig] is what it means.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    files: Files,
    scripts: Scripts,
    redact: Redact,
    limits: Limits,
    audit: Audit,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Files {
    allow: Vec<String>,
    deny: Vec<String>,
    max_bytes: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Scripts {
    packs: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Redact {
    names: Vec<String>,
    patterns: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Limits {
    max_lines: Option<i64>,
    scan_lines: Option<i64>,
    max_response: Option<i64>,
    concurrency: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Audit {
    path: Option<String>,
    max_bytes: Option<i64>,
}

impl NodeConfig {
    /// limen's own files, wherever the configuration puts them: never readable through limen.
    pub fn private_paths(&self) -> Vec<String> {
        vec![self.audit.clone(), format!("{}.1", self.audit)]
    }

    pub fn parse(text: &str) -> ConfigResult<NodeConfig> {
        let file: File = super::from_str(text)?;
        let defaults = NodeConfig::default();
        check_globs(&file.files)?;
        check_redact(&file.redact)?;
        let audit_max_bytes = match file.audit.max_bytes {
            Some(bytes) if bytes < 1024 => return fail("audit.max_bytes", "at least 1024"),
            bytes => bytes.map_or(defaults.audit_max_bytes, |bytes| bytes as u64),
        };
        let packs = file
            .scripts
            .packs
            .into_iter()
            .map(|pack| absolute("scripts.packs", Some(pack)).map(Option::unwrap_or_default));
        Ok(NodeConfig {
            max_file_bytes: positive("files.max_bytes", file.files.max_bytes)?.unwrap_or(defaults.max_file_bytes),
            max_lines: positive("limits.max_lines", file.limits.max_lines)?.unwrap_or(defaults.max_lines),
            scan_lines: positive("limits.scan_lines", file.limits.scan_lines)?.unwrap_or(defaults.scan_lines),
            max_response_bytes: positive("limits.max_response", file.limits.max_response)?
                .unwrap_or(defaults.max_response_bytes),
            concurrency: positive("limits.concurrency", file.limits.concurrency)?.unwrap_or(defaults.concurrency),
            allow: file.files.allow,
            deny: file.files.deny,
            packs: packs.collect::<ConfigResult<_>>()?,
            redact_names: file.redact.names,
            redact_patterns: file.redact.patterns,
            audit: absolute("audit.path", file.audit.path)?.unwrap_or(defaults.audit),
            audit_max_bytes,
        })
    }
}

fn check_globs(files: &Files) -> ConfigResult<()> {
    for (key, globs) in [("files.allow", &files.allow), ("files.deny", &files.deny)] {
        if let Some(error) = globs.iter().find_map(|glob| Glob::new(glob).err()) {
            return fail(key, error);
        }
    }
    Ok(())
}

fn check_redact(redact: &Redact) -> ConfigResult<()> {
    static NAME: LazyLock<Regex> = LazyLock::new(|| own_regex("^[A-Za-z0-9_.-]{1,128}$"));
    if let Some(bad) = redact.names.iter().find(|name| !NAME.is_match(name)) {
        return fail("redact.names", format!("'{bad}' is not a name: letters, digits, `_`, `.` and `-`"));
    }
    if let Some(bad) = redact.patterns.iter().find(|pattern| Regex::new(pattern).is_err()) {
        return fail("redact.patterns", format!("bad regex '{bad}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(NodeConfig::parse("").unwrap(), NodeConfig::default());
        let config = NodeConfig::parse(
            "[files]\nallow = [\"/etc/nginx/**\"]\nmax_bytes = 1024\n[limits]\nmax_lines = 10\n\
             [scripts]\npacks = [\"/opt/state/packs/docker\"]\n[redact]\nnames = [\"DB_PASSWORD\"]",
        )
        .unwrap();
        assert_eq!(config.allow, ["/etc/nginx/**"]);
        assert_eq!(config.max_file_bytes, 1024);
        assert_eq!(config.max_lines, 10);
        assert_eq!(config.packs, ["/opt/state/packs/docker"]);
        assert_eq!(config.redact_names, ["DB_PASSWORD"]);
    }

    #[test]
    fn rejects_mistakes() {
        for (text, expected) in [
            ("[files]\nalow = []", "line 2, column 1: unknown field `alow`"),
            ("[files]\nallow = [\"etc/*\"]", "files.allow"),
            ("[files]\nallow = [1]", "line 2"),
            ("[scripts]\npacks = [\"relative\"]", "scripts.packs: must be an absolute path"),
            ("[scripts]\nchecks = \"/etc/limen/checks.d\"", "line 2, column 1: unknown field `checks`"),
            ("[redact]\npatterns = [\"(\"]", "redact.patterns: bad regex"),
            ("[redact]\nnames = [\"A B\"]", "redact.names: 'A B' is not a name"),
            ("[limits]\nmax_lines = 0", "limits.max_lines: must be positive"),
        ] {
            let error = NodeConfig::parse(text).unwrap_err();
            assert!(error.0.starts_with(expected), "{error} should start with {expected}");
        }
    }
}
