//! `/etc/limen/limen.toml` (spec §7.1). Every key has a default; a missing file means nothing is readable.

use super::{ConfigError, ConfigResult, absolute, fail, positive};
use crate::glob::Glob;
use crate::scripts::ScriptKind;
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;
use toml_edit::{DocumentMut, Item, Table, value};

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
        static CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\w+://)[^/@]*@").unwrap());
        CREDENTIALS.replace(&self.url, "$1").into_owned()
    }

    /// Owner and name when the repository is on GitHub, for the token link of `install`.
    pub fn github(&self) -> Option<(String, String)> {
        static GITHUB: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"^(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)([^/]+)/([^/]+?)(?:\.git)?/?$",
            )
            .unwrap()
        });
        GITHUB.captures(&self.url).map(|c| (c[1].to_string(), c[2].to_string()))
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

/// The file as written; [NodeConfig] is what it means.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    files: Files,
    logs: Logs,
    limits: Limits,
    redact: Redact,
    scripts: Scripts,
    audit: Audit,
    repo: Option<Repo>,
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
struct Logs {
    max_lines: Option<i64>,
    scan_lines: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Limits {
    max_response: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Redact {
    patterns: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Scripts {
    checks: Option<String>,
    actions: Option<String>,
    setup: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Audit {
    path: Option<String>,
    max_bytes: Option<i64>,
    runs: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repo {
    url: Option<String>,
    branch: Option<String>,
    path: Option<String>,
    dir: Option<String>,
    token_file: Option<String>,
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

    pub fn parse(text: &str) -> ConfigResult<NodeConfig> {
        let f: File = super::from_str(text)?;
        let d = NodeConfig::default();
        for (key, list) in [("files.allow", &f.files.allow), ("files.deny", &f.files.deny)] {
            if let Some(e) = list.iter().find_map(|p| Glob::new(p).err()) {
                return fail(key, e);
            }
        }
        if let Some(p) = f.redact.patterns.iter().find(|p| Regex::new(p).is_err()) {
            return fail("redact.patterns", format!("bad regex '{p}'"));
        }
        let audit_max_bytes = match f.audit.max_bytes {
            Some(n) if n < 1024 => return fail("audit.max_bytes", "at least 1024"),
            n => n.map_or(d.audit_max_bytes, |n| n as u64),
        };
        Ok(NodeConfig {
            max_file_bytes: positive("files.max_bytes", f.files.max_bytes)?.unwrap_or(d.max_file_bytes),
            max_lines: positive("logs.max_lines", f.logs.max_lines)?.unwrap_or(d.max_lines),
            scan_lines: positive("logs.scan_lines", f.logs.scan_lines)?.unwrap_or(d.scan_lines),
            max_response_bytes: positive("limits.max_response", f.limits.max_response)?.unwrap_or(d.max_response_bytes),
            allow: f.files.allow,
            deny: f.files.deny,
            redact: f.redact.patterns,
            explicit_checks: absolute("scripts.checks", f.scripts.checks)?,
            explicit_actions: absolute("scripts.actions", f.scripts.actions)?,
            explicit_setup: absolute("scripts.setup", f.scripts.setup)?,
            audit: absolute("audit.path", f.audit.path)?.unwrap_or(d.audit),
            audit_max_bytes,
            runs: absolute("audit.runs", f.audit.runs)?.unwrap_or(d.runs),
            repo: f.repo.map(repo).transpose()?,
        })
    }
}

fn repo(r: Repo) -> ConfigResult<RepoConfig> {
    static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(https://|ssh://|git@|file://)\S+$").unwrap());
    static BRANCH: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[A-Za-z0-9._/-]{1,100}$").unwrap());
    static FOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*)?$").unwrap());
    let Some(url) = r.url else { return fail("repo.url", "missing") };
    if !URL.is_match(&url) {
        return fail("repo.url", "expected an https://, ssh://, git@ or file:// URL");
    }
    let mut repo = RepoConfig::new(&url);
    if let Some(branch) = r.branch {
        if !BRANCH.is_match(&branch) || branch.starts_with('-') {
            return fail("repo.branch", "not a branch name");
        }
        repo.branch = branch;
    }
    let path = r.path.unwrap_or_default().trim_matches('/').to_string();
    if !FOLDER.is_match(&path) || path.split('/').any(|s| s == ".." || s == ".") {
        return fail("repo.path", "a relative folder of the repository");
    }
    repo.path = path;
    if let Some(dir) = absolute("repo.dir", r.dir)? {
        let dir = dir.trim_end_matches('/').to_string();
        if dir.matches('/').count() < 2 {
            return fail("repo.dir", "too close to /; it is replaced on every sync");
        }
        repo.dir = dir;
    }
    if let Some(token_file) = absolute("repo.token_file", r.token_file)? {
        repo.token_file = token_file;
    }
    Ok(repo)
}

/// [text] with its `[repo]` set to these values, what `install` and `join` write; everything else in it, comments
/// included, stays. The folder can come from the hub's name for the node: written as a value, it can't be more.
pub fn with_repo(text: &str, url: &str, branch: &str, path: &str) -> ConfigResult<String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| ConfigError(format!("not TOML: {e}")))?;
    let mut repo = Table::new();
    repo["url"] = value(url);
    repo["branch"] = value(branch);
    repo["path"] = value(path.trim_matches('/'));
    doc["repo"] = Item::Table(repo);
    Ok(doc.to_string())
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
            ("[files]\nalow = []", "line 2, column 1: unknown field `alow`"),
            ("[files]\nallow = [\"etc/*\"]", "files.allow"),
            ("[files]\nallow = [1]", "line 2"),
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
    fn a_repo_is_added_and_nothing_else_changes() {
        let before = "# mine\n[files]\nallow = [\"/etc/**\"] # this too\n";
        let after = with_repo(before, "https://github.com/you/infra.git", "main", "nodes/nas/").unwrap();
        assert!(after.starts_with(before), "{after}");
        let c = NodeConfig::parse(&after).unwrap();
        assert_eq!((c.allow.len(), c.repo.unwrap().path.as_str()), (1, "nodes/nas"));
        // A hub that names the node like this must not point the checks at the actions, or `dir` at /usr/lib.
        for hostile in ["x\"\n[scripts]\nchecks = \"/opt/limen/repo/nodes/x/actions\"\n#", "x\"\ndir = \"/usr/lib\"\n#"]
        {
            let text = with_repo("", "https://github.com/you/infra.git", "main", &format!("nodes/{hostile}")).unwrap();
            let e = NodeConfig::parse(&text).unwrap_err();
            assert!(e.0.starts_with("repo.path"), "{e}");
        }
    }
}
