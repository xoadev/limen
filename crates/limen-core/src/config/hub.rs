//! `$LIMEN_HOME/limen.toml` (spec §7.2): how the hub reaches its nodes and how it listens.

use super::{ConfigResult, fail};
use crate::durations;
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

/// Whether [text] matches [pattern], one of this module's: each compiled once.
pub fn is(pattern: &'static str, text: &str) -> bool {
    static COMPILED: LazyLock<Mutex<HashMap<&'static str, Regex>>> = LazyLock::new(Default::default);
    let mut compiled = COMPILED.lock().unwrap();
    compiled.entry(pattern).or_insert_with(|| Regex::new(pattern).expect("limen's own patterns compile")).is_match(text)
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
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn parse(text: &str) -> ConfigResult<HubConfig> {
        let f: File = super::from_str(text)?;
        let d = HubConfig::default();
        let mut nodes = Vec::with_capacity(f.nodes.len());
        for (name, n) in f.nodes {
            let key = |k: &str| format!("nodes.{name}.{k}");
            if !is(NODE_NAME, &name) {
                return fail(&format!("nodes.{name}"), format!("a node name matches {NODE_NAME}"));
            }
            let Some(host) = n.host.filter(|h| is(HOST, h)) else {
                return fail(&key("host"), "missing, or not a host name or address");
            };
            let port = match n.port.unwrap_or(22) {
                p @ 1..=65535 => p as u16,
                _ => return fail(&key("port"), "out of range"),
            };
            let user = n.user.unwrap_or_else(|| READ_USER.into());
            if !is(USER, &user) {
                return fail(&key("user"), "not a user name");
            }
            let Some(host_key) = n.host_key.map(|k| k.trim().to_string()) else {
                return fail(&key("host_key"), "missing; get it with ssh-keyscan and verify it");
            };
            if !is(HOST_KEY, &host_key) {
                return fail(&key("host_key"), "expected '<type> <base64>', as in known_hosts without the host");
            }
            nodes.push(NodeEntry { name, host, port, user, host_key });
        }
        let per_node_concurrency = match f.ssh.per_node_concurrency {
            Some(n @ 1..=64) => n as usize,
            Some(_) => return fail("ssh.per_node_concurrency", "must be between 1 and 64"),
            None => d.per_node_concurrency,
        };
        if let Some(listen) = f.http.listen.as_ref().filter(|l| !is(r"^\S+:[0-9]{1,5}$", l)) {
            return fail("http.listen", format!("'{listen}': expected host:port"));
        }
        let public_url = f.http.public_url.map(|u| u.trim_end_matches('/').to_string());
        if public_url.as_ref().is_some_and(|u| !is(PUBLIC_URL, u)) {
            return fail("http.public_url", "expected http://<address>:<port>, an address and not a name");
        }
        Ok(HubConfig {
            identity: f.ssh.identity.unwrap_or(d.identity),
            connect_timeout: duration("ssh.connect_timeout", f.ssh.connect_timeout)?.unwrap_or(d.connect_timeout),
            request_timeout: duration("ssh.request_timeout", f.ssh.request_timeout)?.unwrap_or(d.request_timeout),
            per_node_concurrency,
            listen: f.http.listen.unwrap_or(d.listen),
            origins: f.http.origins,
            public_url,
            nodes,
        })
    }
}

fn duration(key: &str, text: Option<String>) -> ConfigResult<Option<Duration>> {
    match text {
        None => Ok(None),
        Some(t) => durations::parse(&t).map(Some).map_or_else(|| fail(key, "expected a duration like 5s or 1m"), Ok),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_and_defaults() {
        let c = HubConfig::parse(
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
        assert_eq!(c.identity, "keys/limen");
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert_eq!(c.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["nas", "router"]);
        assert_eq!(c.node("nas").unwrap().port, 22);
        assert_eq!(c.node("nas").unwrap().user, "limen-read");
        assert_eq!(c.node("router").unwrap().user, "reader");
        // Only the local machine unless told otherwise.
        assert_eq!(c.listen, "127.0.0.1:7341");
    }

    #[test]
    fn a_node_needs_a_pinned_host_key() {
        let e = HubConfig::parse("[nodes.nas]\nhost = \"h\"").unwrap_err();
        assert!(e.0.contains("nodes.nas.host_key: missing"), "{e}");
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"h ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.Nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"").is_err());
        assert!(HubConfig::parse("[nodes.nas]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"\ncolour = 1").is_err());
    }
}
