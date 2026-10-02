//! `$LIMEN_HOME/limen.toml` (spec §7.2): how the hub reaches its nodes and how it listens.

use super::{ConfigResult, fail};
use crate::durations;
use crate::own_regex;
use crate::scripts;
use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub const NODE_USER: &str = "limen";
pub const NODE_NAME: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";
pub const HOST_KEY: &str =
    r"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com) [A-Za-z0-9+/]+=*$";
pub const HOST: &str = "^[A-Za-z0-9.:_-]{1,253}$";
pub const USER: &str = "^[a-z_][a-z0-9_-]{0,31}$";
pub const PUBLIC_URL: &str = r"^http://(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]):\d{1,5}$";
const LISTEN: &str = r"^\S+:[0-9]{1,5}$";

/// Whether [text] matches [pattern], one of this module's: each compiled once.
pub fn is(pattern: &'static str, text: &str) -> bool {
    static COMPILED: LazyLock<Mutex<HashMap<&'static str, Regex>>> = LazyLock::new(Default::default);
    let mut compiled = COMPILED.lock().expect("held only to compile limen's own patterns, which compile");
    compiled.entry(pattern).or_insert_with(|| own_regex(pattern)).is_match(text)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubConfig {
    pub identity: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub per_node_concurrency: usize,
    pub listen: String,
    pub origins: Vec<String>,
    /// Where nodes reach this hub to join (spec §10.1): an address, not a name.
    pub public_url: Option<String>,
    pub nodes: Vec<NodeEntry>,
    pub approval: Approval,
}

/// Which scripts a person approves before each run, and how long a call waits for the answer (spec §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub scripts: ApprovalScripts,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApprovalScripts {
    #[default]
    None,
    /// Every script whose tool isn't read-only.
    Changes,
    Named(Vec<String>),
}

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const APPROVAL_TIMEOUTS: std::ops::RangeInclusive<Duration> = Duration::from_secs(10)..=Duration::from_secs(3600);

impl Default for Approval {
    fn default() -> Self {
        Self { scripts: ApprovalScripts::None, timeout: APPROVAL_TIMEOUT }
    }
}

impl Approval {
    /// Whether a run of [script], whose tool is [read_only] or not, waits for a person's yes.
    pub fn needed(&self, script: &str, read_only: bool) -> bool {
        match &self.scripts {
            ApprovalScripts::None => false,
            ApprovalScripts::Changes => !read_only,
            ApprovalScripts::Named(names) => names.iter().any(|name| name == script),
        }
    }
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            identity: "id_ed25519".into(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            per_node_concurrency: 4,
            listen: "127.0.0.1:7341".into(),
            origins: vec![],
            public_url: None,
            nodes: vec![],
            approval: Approval::default(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    ssh: Ssh,
    http: Http,
    approval: ApprovalFile,
    nodes: IndexMap<String, Node>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ApprovalFile {
    scripts: Option<ScriptsFile>,
    timeout: Option<String>,
}

/// `approval.scripts` as written: a word or a list of names.
#[derive(Deserialize)]
#[serde(untagged)]
enum ScriptsFile {
    Word(String),
    Names(Vec<String>),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Ssh {
    identity: Option<String>,
    connect_timeout: Option<String>,
    request_timeout: Option<String>,
    per_node_concurrency: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Http {
    listen: Option<String>,
    origins: Vec<String>,
    public_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    host: Option<String>,
    port: Option<i64>,
    user: Option<String>,
    host_key: Option<String>,
}

impl HubConfig {
    pub fn node(&self, name: &str) -> Option<&NodeEntry> {
        self.nodes.iter().find(|node| node.name == name)
    }

    /// `ssh.identity` as a path: absolute, or under the hub's directory [home]. Its public key is this with `.pub`.
    pub fn identity_path(&self, home: &str) -> String {
        if self.identity.starts_with('/') {
            self.identity.clone()
        } else {
            format!("{}/{}", home.trim_end_matches('/'), self.identity)
        }
    }

    /// `ssh.connect_timeout` as ssh's `ConnectTimeout` takes it: whole seconds, rounded up.
    pub fn connect_timeout_seconds(&self) -> u128 {
        self.connect_timeout.as_millis().div_ceil(1000)
    }

    /// Where `serve` listens: [option] (`--listen`), else [env] (`LIMEN_LISTEN`), else `http.listen`; any of them
    /// checked as the file's key is.
    pub fn listen_address(&self, option: Option<String>, env: Option<String>) -> ConfigResult<String> {
        match (option, env) {
            (Some(address), _) => checked_listen("--listen", address),
            (None, Some(address)) => checked_listen("LIMEN_LISTEN", address),
            (None, None) => Ok(self.listen.clone()),
        }
    }

    pub fn parse(text: &str) -> ConfigResult<HubConfig> {
        let file: File = super::from_str(text)?;
        let defaults = HubConfig::default();
        let nodes = file.nodes.into_iter().map(|(name, node)| node_entry(name, node)).collect::<ConfigResult<_>>()?;
        let per_node_concurrency = match file.ssh.per_node_concurrency {
            Some(count @ 1..=64) => count as usize,
            Some(_) => return fail("ssh.per_node_concurrency", "must be between 1 and 64"),
            None => defaults.per_node_concurrency,
        };
        let listen = file.http.listen.map(|listen| checked_listen("http.listen", listen)).transpose()?;
        let public_url = public_url(file.http.public_url)?;
        if file.ssh.identity.as_deref().is_some_and(|identity| identity.is_empty() || identity.ends_with('/')) {
            return fail("ssh.identity", "a key file: relative to the hub's directory, or absolute");
        }
        Ok(HubConfig {
            identity: file.ssh.identity.unwrap_or(defaults.identity),
            connect_timeout: duration("ssh.connect_timeout", file.ssh.connect_timeout)?
                .unwrap_or(defaults.connect_timeout),
            request_timeout: duration("ssh.request_timeout", file.ssh.request_timeout)?
                .unwrap_or(defaults.request_timeout),
            per_node_concurrency,
            listen: listen.unwrap_or(defaults.listen),
            origins: file.http.origins,
            public_url,
            nodes,
            approval: approval(file.approval)?,
        })
    }
}

/// `[approval]` checked: a known word or script names, and a timeout a person can answer within.
fn approval(file: ApprovalFile) -> ConfigResult<Approval> {
    let scripts = match file.scripts {
        None => ApprovalScripts::None,
        Some(ScriptsFile::Word(word)) if word == "none" => ApprovalScripts::None,
        Some(ScriptsFile::Word(word)) if word == "changes" => ApprovalScripts::Changes,
        Some(ScriptsFile::Word(_)) => {
            return fail("approval.scripts", "\"none\", \"changes\" or a list of script names");
        }
        Some(ScriptsFile::Names(names)) => {
            if let Some(bad) = names.iter().find(|name| !scripts::is_script_name(name)) {
                return fail("approval.scripts", format!("'{bad}' is not a script's name"));
            }
            ApprovalScripts::Named(names)
        }
    };
    let timeout = duration("approval.timeout", file.timeout)?.unwrap_or(APPROVAL_TIMEOUT);
    if !APPROVAL_TIMEOUTS.contains(&timeout) {
        return fail("approval.timeout", "10s to 1h");
    }
    Ok(Approval { scripts, timeout })
}

/// `[nodes.<name>]` checked: a host, a port, a user and the pinned host key.
fn node_entry(name: String, node: Node) -> ConfigResult<NodeEntry> {
    let key = |field: &str| format!("nodes.{name}.{field}");
    if !is(NODE_NAME, &name) {
        return fail(&format!("nodes.{name}"), format!("a node name matches {NODE_NAME}"));
    }
    let Some(host) = node.host.filter(|host| is(HOST, host)) else {
        return fail(&key("host"), "missing, or not a host name or address");
    };
    let port = match node.port.unwrap_or(22) {
        port @ 1..=65535 => port as u16,
        _ => return fail(&key("port"), "out of range"),
    };
    let user = node.user.unwrap_or_else(|| NODE_USER.into());
    if !is(USER, &user) {
        return fail(&key("user"), "not a user name");
    }
    let Some(host_key) = node.host_key.map(|host_key| host_key.trim().to_string()) else {
        return fail(&key("host_key"), "missing; get it with ssh-keyscan and verify it");
    };
    if !is(HOST_KEY, &host_key) {
        return fail(&key("host_key"), "expected '<type> <base64>', as in known_hosts without the host");
    }
    Ok(NodeEntry { name, host, port, user, host_key })
}

/// [address] if it is `host:port`; [key] says where it came from.
fn checked_listen(key: &str, address: String) -> ConfigResult<String> {
    if !is(LISTEN, &address) {
        return fail(key, format!("'{address}': expected host:port"));
    }
    Ok(address)
}

/// `[http].public_url` without a trailing slash, if it is an address.
fn public_url(url: Option<String>) -> ConfigResult<Option<String>> {
    let url = url.map(|url| url.trim_end_matches('/').to_string());
    if url.as_ref().is_some_and(|url| !is(PUBLIC_URL, url)) {
        return fail("http.public_url", "expected http://<address>:<port>, an address and not a name");
    }
    Ok(url)
}

fn duration(key: &str, text: Option<String>) -> ConfigResult<Option<Duration>> {
    let Some(text) = text else { return Ok(None) };
    match durations::parse(&text) {
        Some(duration) if duration.is_zero() => fail(key, "must be more than zero"),
        Some(duration) => Ok(Some(duration)),
        None => fail(key, "expected a duration like 5s or 1m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_and_defaults() {
        let config = HubConfig::parse(
            r#"[ssh]
identity = "keys/limen"
connect_timeout = "3s"

[nodes.nas]
host = "100.64.0.2"
host_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx"

[nodes.router]
host = "router.lan"
port = 2222
user = "reader"
host_key = "ecdsa-sha2-nistp256 AAAAE2VjZHNh="
"#,
        )
        .unwrap();
        assert_eq!(config.identity, "keys/limen");
        assert_eq!(config.connect_timeout, Duration::from_secs(3));
        assert_eq!(config.nodes.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(), ["nas", "router"]);
        assert_eq!(config.node("nas").unwrap().port, 22);
        assert_eq!(config.node("nas").unwrap().user, "limen");
        assert_eq!(config.node("router").unwrap().user, "reader");
        // Only the local machine unless told otherwise.
        assert_eq!(config.listen, "127.0.0.1:7341");
    }

    #[test]
    fn listen_comes_from_the_option_the_environment_or_the_file_each_checked() {
        let config = HubConfig::parse("[http]\nlisten = \"127.0.0.1:1\"").unwrap();
        let given = |text: &str| Some(text.to_string());
        assert_eq!(config.listen_address(None, None).unwrap(), "127.0.0.1:1");
        assert_eq!(config.listen_address(None, given("0.0.0.0:2")).unwrap(), "0.0.0.0:2");
        assert_eq!(config.listen_address(given("[::]:3"), given("0.0.0.0:2")).unwrap(), "[::]:3");
        let error = config.listen_address(None, given("7341")).unwrap_err();
        assert_eq!(error.0, "LIMEN_LISTEN: '7341': expected host:port");
        let error = config.listen_address(given("0.0.0.0"), None).unwrap_err();
        assert_eq!(error.0, "--listen: '0.0.0.0': expected host:port");
        let error = HubConfig::parse("[http]\nlisten = \"localhost\"").unwrap_err();
        assert_eq!(error.0, "http.listen: 'localhost': expected host:port");
    }

    #[test]
    fn timeouts_are_more_than_zero_and_connecting_takes_whole_seconds() {
        for key in ["connect_timeout", "request_timeout"] {
            for zero in ["0s", "0ms"] {
                let error = HubConfig::parse(&format!("[ssh]\n{key} = \"{zero}\"")).unwrap_err();
                assert_eq!(error.0, format!("ssh.{key}: must be more than zero"));
            }
        }
        let connect = |timeout: &str| {
            HubConfig::parse(&format!("[ssh]\nconnect_timeout = \"{timeout}\"")).unwrap().connect_timeout_seconds()
        };
        assert_eq!([connect("1ms"), connect("1500ms"), connect("2s")], [1, 2, 2]);
    }

    #[test]
    fn the_identity_is_under_the_hub_unless_absolute() {
        let identity = |text: &str| HubConfig::parse(text).map(|config| config.identity_path("/data/"));
        assert_eq!(identity("").unwrap(), "/data/id_ed25519");
        assert_eq!(identity("[ssh]\nidentity = \"keys/limen\"").unwrap(), "/data/keys/limen");
        assert_eq!(identity("[ssh]\nidentity = \"/etc/limen-hub/key\"").unwrap(), "/etc/limen-hub/key");
        for not_a_file in ["", "keys/"] {
            let error = identity(&format!("[ssh]\nidentity = \"{not_a_file}\"")).unwrap_err();
            assert!(error.0.starts_with("ssh.identity: "), "{error}");
        }
    }

    #[test]
    fn a_node_needs_a_pinned_host_key() {
        let error = HubConfig::parse("[nodes.nas]\nhost = \"h\"").unwrap_err();
        assert!(error.0.contains("nodes.nas.host_key: missing"), "{error}");
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"h ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.Nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"\ncolour = 1").is_err());
    }

    #[test]
    fn nothing_needs_approval_unless_the_file_says_so() {
        let approval = HubConfig::parse("").unwrap().approval;
        assert!(!approval.needed("upgrade", false));
        assert_eq!(approval.timeout, Duration::from_secs(300));
    }

    #[test]
    fn approval_of_every_change_or_of_named_scripts() {
        let changes = HubConfig::parse("[approval]\nscripts = \"changes\"\ntimeout = \"2m\"").unwrap().approval;
        assert!(changes.needed("upgrade", false));
        assert!(!changes.needed("status", true), "a read-only script runs without asking");
        assert_eq!(changes.timeout, Duration::from_secs(120));
        let named = HubConfig::parse("[approval]\nscripts = [\"reboot\", \"status\"]").unwrap().approval;
        assert!(named.needed("reboot", false));
        assert!(named.needed("status", true), "a script named is asked for, read-only or not");
        assert!(!named.needed("upgrade", false));
    }

    #[test]
    fn approval_settings_are_checked() {
        let refused = |text: &str| HubConfig::parse(&format!("[approval]\n{text}")).unwrap_err().0;
        assert_eq!(refused("scripts = \"all\""), "approval.scripts: \"none\", \"changes\" or a list of script names");
        assert_eq!(refused("scripts = [\"Bad name\"]"), "approval.scripts: 'Bad name' is not a script's name");
        assert_eq!(refused("timeout = \"5s\""), "approval.timeout: 10s to 1h");
        assert_eq!(refused("timeout = \"2h\""), "approval.timeout: 10s to 1h");
        assert!(refused("colour = 1").contains("unknown field"));
    }
}
