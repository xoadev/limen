//! `$LIMEN_HOME/limen.toml` (spec §7.2): how the hub reaches its nodes and how it listens.

use crate::durations;
use crate::toml_reader::{self, Reader, TomlResult};
use regex::Regex;
use std::time::Duration;

pub const READ_USER: &str = "limen-read";
pub const NODE_NAME: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";
pub const HOST_KEY: &str =
    r"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com) [A-Za-z0-9+/]+=*$";
pub const HOST: &str = "^[A-Za-z0-9.:_-]{1,253}$";
pub const USER: &str = "^[a-z_][a-z0-9_-]{0,31}$";
pub const PUBLIC_URL: &str = r"^http://(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]):\d{1,5}$";

pub fn is(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).unwrap().is_match(text)
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

impl HubConfig {
    pub fn node(&self, name: &str) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn parse(text: &str) -> TomlResult<HubConfig> {
        let table = toml_reader::parse(text)?;
        let root = Reader::new(&table);
        let mut config = HubConfig::default();
        let ssh = root.table("ssh")?;
        let http = root.table("http")?;
        if let Some(nodes) = root.table("nodes")? {
            for (name, t) in nodes.tables()? {
                if !is(NODE_NAME, &name) {
                    return nodes.fail(&name, &format!("a node name matches {NODE_NAME}"));
                }
                let Some(host) = t.string("host")? else { return t.fail("host", "missing") };
                if !is(HOST, &host) {
                    return t.fail("host", "not a host name or address");
                }
                let port = t.int("port")?.unwrap_or(22);
                if !(1..=65535).contains(&port) {
                    return t.fail("port", "out of range");
                }
                let user = t.string("user")?.unwrap_or_else(|| READ_USER.into());
                if !is(USER, &user) {
                    return t.fail("user", "not a user name");
                }
                let Some(key) = t.string("host_key")? else {
                    return t.fail("host_key", "missing; get it with ssh-keyscan and verify it");
                };
                let key = key.trim().to_string();
                if !is(HOST_KEY, &key) {
                    return t.fail("host_key", "expected '<type> <base64>', as in known_hosts without the host");
                }
                t.reject_unknown()?;
                config.nodes.push(NodeEntry { name, host, port: port as u16, user, host_key: key });
            }
        }
        if let Some(s) = &ssh {
            config.identity = s.string("identity")?.unwrap_or(config.identity);
            config.connect_timeout = duration(s, "connect_timeout")?.unwrap_or(config.connect_timeout);
            config.request_timeout = duration(s, "request_timeout")?.unwrap_or(config.request_timeout);
            if let Some(n) = s.int("per_node_concurrency")? {
                if !(1..=64).contains(&n) {
                    return s.fail("per_node_concurrency", "must be between 1 and 64");
                }
                config.per_node_concurrency = n as usize;
            }
        }
        if let Some(h) = &http {
            if let Some(listen) = h.string("listen")? {
                if !is(r"^\S+:[0-9]{1,5}$", &listen) {
                    return h.fail("listen", "expected host:port");
                }
                config.listen = listen;
            }
            config.origins = h.strings("origins")?.unwrap_or_default();
            if let Some(url) = h.string("public_url")? {
                let url = url.trim_end_matches('/').to_string();
                if !is(PUBLIC_URL, &url) {
                    return h.fail("public_url", "expected http://<address>:<port>, an address and not a name");
                }
                config.public_url = Some(url);
            }
        }
        for t in [&ssh, &http].into_iter().flatten() {
            t.reject_unknown()?;
        }
        root.reject_unknown()?;
        Ok(config)
    }
}

fn duration(t: &Reader, key: &str) -> TomlResult<Option<Duration>> {
    match t.string(key)? {
        None => Ok(None),
        Some(s) => match durations::parse(&s) {
            Some(d) => Ok(Some(d)),
            None => t.fail(key, "expected a duration like 5s or 1m"),
        },
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
    }
}
