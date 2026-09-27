//! `/etc/limen/limen.toml` (spec §7.1). Every key has a default; a missing file means nothing is readable.

use crate::glob::Glob;
use crate::scripts::ScriptKind;
use crate::toml_reader::{self, Reader, TomlResult, quote};
use regex::Regex;

pub const PATH: &str = "/etc/limen/limen.toml";

/// `[repo]`: the Git repository a node takes its scripts, stacks and expected state from (spec §6.1). `path` is the
/// node's folder inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub url: String,
    pub branch: String,
    pub path: String,
    pub dir: String,
    pub token_file: String,
}

impl RepoConfig {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.into(),
            branch: "main".into(),
            path: String::new(),
            dir: "/opt/limen/repo".into(),
            token_file: "/etc/limen/repo-token".into(),
        }
    }

    /// The node's folder in the checkout.
    pub fn base(&self) -> String {
        if self.path.is_empty() { self.dir.clone() } else { format!("{}/{}", self.dir, self.path) }
    }

    /// The URL without credentials, for answers and logs.
    pub fn display_url(&self) -> String {
        Regex::new(r"^(\w+://)[^/@]*@").unwrap().replace(&self.url, "$1").into_owned()
    }

    /// Owner and name when the repository is on GitHub, for the token link of `install`.
    pub fn github(&self) -> Option<(String, String)> {
        Regex::new(r"^(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)([^/]+)/([^/]+?)(?:\.git)?/?$")
            .unwrap()
            .captures(&self.url)
            .map(|c| (c[1].to_string(), c[2].to_string()))
    }

    /// Last `sync`: next to the checkout, outside it, so `git clean` never removes it.
    pub fn sync_record(&self) -> String {
        let dir = self.dir.trim_end_matches('/');
        format!("{}/last-sync.json", dir.rsplit_once('/').map_or("", |(parent, _)| parent))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub max_file_bytes: usize,
    pub max_lines: usize,
    pub scan_lines: usize,
    pub max_response_bytes: usize,
    pub redact: Vec<String>,
    pub explicit_checks: Option<String>,
    pub explicit_actions: Option<String>,
    pub explicit_setup: Option<String>,
    pub audit: String,
    pub audit_max_bytes: u64,
    pub runs: String,
    pub repo: Option<RepoConfig>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            allow: vec![],
            deny: vec![],
            max_file_bytes: 256 * 1024,
            max_lines: 2000,
            scan_lines: 100_000,
            max_response_bytes: 1024 * 1024,
            redact: vec![],
            explicit_checks: None,
            explicit_actions: None,
            explicit_setup: None,
            audit: "/var/log/limen/audit.jsonl".into(),
            audit_max_bytes: 5 * 1024 * 1024,
            runs: "/var/log/limen/runs".into(),
            repo: None,
        }
    }
}

impl NodeConfig {
    /// A script directory: what `[scripts]` says, else the repository's folder, else `/etc/limen/<kind>.d`.
    pub fn directory(&self, kind: ScriptKind) -> String {
        let (explicit, folder, default) = match kind {
            ScriptKind::Check => (&self.explicit_checks, "checks", "/etc/limen/checks.d"),
            ScriptKind::Action => (&self.explicit_actions, "actions", "/etc/limen/actions.d"),
            ScriptKind::Setup => (&self.explicit_setup, "setup", "/etc/limen/setup.d"),
        };
        explicit
            .clone()
            .or_else(|| self.repo.as_ref().map(|r| format!("{}/{folder}", r.base())))
            .unwrap_or_else(|| default.into())
    }

    /// `stacks/<name>/compose.yaml` in the node's folder of the repository.
    pub fn stacks(&self) -> Option<String> {
        self.repo.as_ref().map(|r| format!("{}/stacks", r.base()))
    }

    /// What must be running: `node.toml` in the node's folder of the repository.
    pub fn expectations(&self) -> Option<String> {
        self.repo.as_ref().map(|r| format!("{}/node.toml", r.base()))
    }

    pub fn parse(text: &str) -> TomlResult<NodeConfig> {
        let table = toml_reader::parse(text)?;
        let root = Reader::new(&table);
        let d = NodeConfig::default();
        let files = root.table("files")?;
        let logs = root.table("logs")?;
        let limits = root.table("limits")?;
        let redact = root.table("redact")?;
        let scripts = root.table("scripts")?;
        let audit = root.table("audit")?;
        let repo = root.table("repo")?;
        let mut config = d.clone();
        if let Some(f) = &files {
            if let Some(allow) = f.strings("allow")? {
                patterns(f, "allow", &allow)?;
                config.allow = allow;
            }
            if let Some(deny) = f.strings("deny")? {
                patterns(f, "deny", &deny)?;
                config.deny = deny;
            }
            if let Some(n) = positive(f, "max_bytes")? {
                config.max_file_bytes = n;
            }
        }
        if let Some(l) = &logs {
            config.max_lines = positive(l, "max_lines")?.unwrap_or(d.max_lines);
            config.scan_lines = positive(l, "scan_lines")?.unwrap_or(d.scan_lines);
        }
        if let Some(l) = &limits {
            config.max_response_bytes = positive(l, "max_response")?.unwrap_or(d.max_response_bytes);
        }
        if let Some(r) = &redact {
            if let Some(list) = r.strings("patterns")? {
                for p in &list {
                    if regex::Regex::new(p).is_err() {
                        return r.fail("patterns", &format!("bad regex '{p}'"));
                    }
                }
                config.redact = list;
            }
        }
        if let Some(s) = &scripts {
            config.explicit_checks = absolute(s, "checks")?;
            config.explicit_actions = absolute(s, "actions")?;
            config.explicit_setup = absolute(s, "setup")?;
        }
        if let Some(a) = &audit {
            config.audit = absolute(a, "path")?.unwrap_or(d.audit.clone());
            if let Some(n) = a.long("max_bytes")? {
                if n < 1024 {
                    return a.fail("max_bytes", "at least 1024");
                }
                config.audit_max_bytes = n as u64;
            }
            config.runs = absolute(a, "runs")?.unwrap_or(d.runs.clone());
        }
        if let Some(r) = &repo {
            config.repo = Some(repo_config(r)?);
        }
        for t in [&files, &logs, &limits, &redact, &scripts, &audit, &repo].into_iter().flatten() {
            t.reject_unknown()?;
        }
        root.reject_unknown()?;
        Ok(config)
    }
}

/// The `[repo]` table `install` and `join` write. The values come from the command line and, for the folder, from
/// the hub's name for the node: quoted, so none of them can write TOML of its own.
pub fn repo_section(url: &str, branch: &str, path: &str) -> String {
    format!("[repo]\nurl = {}\nbranch = {}\npath = {}\n", quote(url), quote(branch), quote(path.trim_matches('/')))
}

fn repo_config(t: &Reader) -> TomlResult<RepoConfig> {
    let url = match t.string("url")? {
        Some(u) => u,
        None => return t.fail("url", "missing"),
    };
    if !Regex::new(r"^(https://|ssh://|git@|file://)\S+$").unwrap().is_match(&url) {
        return t.fail("url", "expected an https://, ssh://, git@ or file:// URL");
    }
    let mut repo = RepoConfig::new(&url);
    if let Some(branch) = t.string("branch")? {
        if !Regex::new("^[A-Za-z0-9._/-]{1,100}$").unwrap().is_match(&branch) || branch.starts_with('-') {
            return t.fail("branch", "not a branch name");
        }
        repo.branch = branch;
    }
    let path = t.string("path")?.unwrap_or_default().trim_matches('/').to_string();
    if !Regex::new(r"^([A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*)?$").unwrap().is_match(&path)
        || path.split('/').any(|s| s == ".." || s == ".")
    {
        return t.fail("path", "a relative folder of the repository");
    }
    repo.path = path;
    if let Some(dir) = absolute(t, "dir")? {
        let dir = dir.trim_end_matches('/').to_string();
        if dir.matches('/').count() < 2 {
            return t.fail("dir", "too close to /; it is replaced on every sync");
        }
        repo.dir = dir;
    }
    if let Some(token_file) = absolute(t, "token_file")? {
        repo.token_file = token_file;
    }
    Ok(repo)
}

fn patterns(t: &Reader, key: &str, list: &[String]) -> TomlResult<()> {
    for p in list {
        if let Err(e) = Glob::new(p) {
            return t.fail(key, &e);
        }
    }
    Ok(())
}

fn positive(t: &Reader, key: &str) -> TomlResult<Option<usize>> {
    match t.int(key)? {
        Some(n) if n <= 0 => t.fail(key, "must be positive"),
        n => Ok(n.map(|n| n as usize)),
    }
}

fn absolute(t: &Reader, key: &str) -> TomlResult<Option<String>> {
    match t.string(key)? {
        Some(v) if !v.starts_with('/') => t.fail(key, "must be an absolute path"),
        v => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(NodeConfig::parse("").unwrap(), NodeConfig::default());
        let c = NodeConfig::parse(
            "[files]\nallow = [\"/etc/nginx/**\"]\nmax_bytes = 1024\n[logs]\nmax_lines = 10\n[scripts]\nchecks = \"/opt/infra/checks\"",
        )
        .unwrap();
        assert_eq!(c.allow, ["/etc/nginx/**"]);
        assert_eq!(c.max_file_bytes, 1024);
        assert_eq!(c.max_lines, 10);
        assert_eq!(c.directory(ScriptKind::Check), "/opt/infra/checks");
        assert_eq!(c.directory(ScriptKind::Action), "/etc/limen/actions.d");
    }

    #[test]
    fn rejects_mistakes() {
        for (text, expected) in [
            ("[files]\nalow = []", "files.alow: unknown key"),
            ("[files]\nallow = [\"etc/*\"]", "files.allow"),
            ("[scripts]\nchecks = \"relative\"", "scripts.checks: must be an absolute path"),
            ("[redact]\npatterns = [\"(\"]", "redact.patterns: bad regex"),
            ("[logs]\nmax_lines = 0", "logs.max_lines: must be positive"),
            ("[repo]\nbranch = \"main\"", "repo.url: missing"),
        ] {
            let e = NodeConfig::parse(text).unwrap_err();
            assert!(e.0.starts_with(expected), "{e} should start with {expected}");
        }
    }

    #[test]
    fn a_repo_follows_the_folder() {
        let c = NodeConfig::parse("[repo]\nurl = \"https://github.com/you/infra.git\"\npath = \"nodes/nas/\"").unwrap();
        let repo = c.repo.as_ref().unwrap();
        assert_eq!(repo.base(), "/opt/limen/repo/nodes/nas");
        assert_eq!(c.directory(ScriptKind::Setup), "/opt/limen/repo/nodes/nas/setup");
        assert_eq!(repo.github(), Some(("you".into(), "infra".into())));
        assert_eq!(repo.sync_record(), "/opt/limen/last-sync.json");
        assert_eq!(RepoConfig::new("https://x:tok@host/r.git").display_url(), "https://host/r.git");
    }

    #[test]
    fn a_repo_section_cannot_write_more_than_itself() {
        let ok = NodeConfig::parse(&repo_section("https://github.com/you/infra.git", "main", "nodes/nas/")).unwrap();
        assert_eq!(ok.repo.unwrap().path, "nodes/nas");
        // A hub that names the node like this must not point the checks at the actions, or `dir` at /usr/lib.
        for hostile in ["x\"\n[scripts]\nchecks = \"/opt/limen/repo/nodes/x/actions\"\n#", "x\"\ndir = \"/usr/lib\"\n#"]
        {
            let e = NodeConfig::parse(&repo_section(
                "https://github.com/you/infra.git",
                "main",
                &format!("nodes/{hostile}"),
            ))
            .unwrap_err();
            assert!(e.0.starts_with("repo.path"), "{e}");
        }
    }
}
