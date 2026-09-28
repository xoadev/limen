//! `$LIMEN_HOME/limen.toml` (spec §7.2): how the hub reaches its nodes and how it listens.

use super::{ConfigResult, fail};
use crate::durations;
use crate::own_regex;
use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub const READ_USER: &str = "limen-read";
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
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    ssh: Ssh,
    http: Http,
    nodes: IndexMap<String, Node>,
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

    pub fn parse(text: &str) -> ConfigResult<HubConfig> {
        let file: File = super::from_str(text)?;
        let defaults = HubConfig::default();
        let nodes = file.nodes.into_iter().map(|(name, node)| node_entry(name, node)).collect::<ConfigResult<_>>()?;
        let per_node_concurrency = match file.ssh.per_node_concurrency {
            Some(count @ 1..=64) => count as usize,
            Some(_) => return fail("ssh.per_node_concurrency", "must be between 1 and 64"),
            None => defaults.per_node_concurrency,
        };
        if let Some(listen) = file.http.listen.as_ref().filter(|listen| !is(LISTEN, listen)) {
            return fail("http.listen", format!("'{listen}': expected host:port"));
        }
        let public_url = public_url(file.http.public_url)?;
        Ok(HubConfig {
            identity: file.ssh.identity.unwrap_or(defaults.identity),
            connect_timeout: duration("ssh.connect_timeout", file.ssh.connect_timeout)?
                .unwrap_or(defaults.connect_timeout),
            request_timeout: duration("ssh.request_timeout", file.ssh.request_timeout)?
                .unwrap_or(defaults.request_timeout),
            per_node_concurrency,
            listen: file.http.listen.unwrap_or(defaults.listen),
            origins: file.http.origins,
            public_url,
            nodes,
        })
    }
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
    let user = node.user.unwrap_or_else(|| READ_USER.into());
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
        assert_eq!(config.node("nas").unwrap().user, "limen-read");
        assert_eq!(config.node("router").unwrap().user, "reader");
        // Only the local machine unless told otherwise.
        assert_eq!(config.listen, "127.0.0.1:7341");
    }

    #[test]
    fn a_node_needs_a_pinned_host_key() {
        let error = HubConfig::parse("[nodes.nas]\nhost = \"h\"").unwrap_err();
        assert!(error.0.contains("nodes.nas.host_key: missing"), "{error}");
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"h ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.Nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"\ncolour = 1").is_err());
    }
}
