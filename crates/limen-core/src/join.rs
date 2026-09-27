//! Joining a node to a hub (spec §10.1): the hub hands out a line with a one-time code, the fingerprint of its key
//! and a secret; the node downloads the key, checks it against the fingerprint, installs, and tells the hub its host
//! key, signed with the secret.

use crate::config::hub::{self, is};
use crate::protocol::{Result, bad_request};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `SHA256:<base64>` of the key blob, as `ssh-keygen -lf` prints it: what a person can check with standard tools.
pub fn fingerprint(public_key: &str) -> Result<String> {
    let re = Regex::new(r"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)) ([A-Za-z0-9+/]+=*)( .*)?$").unwrap();
    let blob =
        re.captures(public_key.trim()).map(|c| c[3].to_string()).ok_or_else(|| bad_request("not an SSH public key"))?;
    let bytes = STANDARD.decode(blob).map_err(|_| bad_request("not an SSH public key"))?;
    Ok(format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes))))
}

/// `<type> <base64>`, without the comment: how a host key is written in the hub's limen.toml.
pub fn without_comment(public_key: &str) -> String {
    public_key.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// `http://100.64.0.2:7341/join/<code>#SHA256:<fingerprint>.<secret>`, the line `limen invite` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinUrl {
    pub base: String,
    pub code: String,
    pub fingerprint: String,
    /// Never sent: it signs the node's arrival, so whoever sees the code on the wire can't arrive in its place.
    pub secret: String,
}

pub const CODE: &str = "^[a-z2-7]{26}$";

impl JoinUrl {
    pub fn parse(text: &str) -> Result<JoinUrl> {
        // An address and not a name: the static binary is not the place to resolve them.
        let re = Regex::new(
            r"^(http://(?:\d{1,3}(?:\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]):\d{1,5})/join/([a-z2-7]{26})#(SHA256:[A-Za-z0-9+/]{43})\.([a-z2-7]{26})$",
        )
        .unwrap();
        let c = re.captures(text.trim()).ok_or_else(|| {
            bad_request("not a join line from `limen invite`: expected http://<address>:<port>/join/<code>#SHA256:<fingerprint>.<secret>")
        })?;
        Ok(JoinUrl { base: c[1].into(), code: c[2].into(), fingerprint: c[3].into(), secret: c[4].into() })
    }

    /// The host and port to connect to: `100.64.0.2:7341`, or `[fd00::1]:7341`.
    pub fn authority(&self) -> &str {
        self.base.trim_start_matches("http://")
    }
}

impl std::fmt::Display for JoinUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/join/{}#{}.{}", self.base, self.code, self.fingerprint, self.secret)
    }
}

/// What `GET /join/<code>` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub name: String,
    pub hub_key: String,
}

/// What a node sends with `POST /join/<code>` once installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arrival {
    pub host_key: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Where the hub reaches it; without it, the address the request came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// HMAC-SHA256 of the fields above with the join line's secret, hex.
    #[serde(default)]
    pub proof: String,
}

fn default_port() -> u16 {
    22
}

impl Arrival {
    pub fn new(host_key: &str, user: &str, port: u16, address: Option<&str>) -> Self {
        Self {
            host_key: host_key.into(),
            user: user.into(),
            port,
            address: address.map(String::from),
            proof: String::new(),
        }
    }

    pub fn proof_with(&self, secret: &str) -> String {
        let fields =
            format!("{}\n{}\n{}\n{}", self.host_key, self.user, self.port, self.address.as_deref().unwrap_or(""));
        hex(&hmac_sha256(secret.as_bytes(), fields.as_bytes()))
    }

    pub fn signed(mut self, secret: &str) -> Self {
        self.proof = self.proof_with(secret);
        self
    }
}

/// What the hub answers to an [Arrival].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub name: String,
    pub address: String,
    pub reachable: bool,
    pub detail: String,
}

/// An invitation as the hub keeps it until it is used or expires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInvite {
    pub name: String,
    pub expires_epoch: i64,
    pub secret: String,
}

/// The hub's limen.toml, edited as text so that everything else in it —comments, order, other settings— stays as the
/// operator wrote it: a node's `[nodes.<name>]` table is replaced or appended, nothing more.
///
/// Every value is checked against the pattern the configuration itself enforces before it is written: they come from
/// the node that joins, and a quote and a newline in an address would otherwise let it write TOML of its own.
pub fn upsert_node(text: &str, name: &str, host: &str, port: u16, user: &str, host_key: &str) -> Result<String> {
    let key = without_comment(host_key);
    if !is(hub::NODE_NAME, name) {
        return Err(bad_request(format!("'{name}' is not a node name")));
    }
    if !is(hub::HOST, host) {
        return Err(bad_request("the address is not an address or host name"));
    }
    if port == 0 {
        return Err(bad_request("the port is out of range"));
    }
    if !is(hub::USER, user) {
        return Err(bad_request("the user is not a user name"));
    }
    if !is(hub::HOST_KEY, &key) {
        return Err(bad_request("the host key is not '<type> <base64>'"));
    }
    let mut section = format!("[nodes.{name}]\nhost = \"{host}\"\n");
    if port != 22 {
        section.push_str(&format!("port = {port}\n"));
    }
    if user != hub::READ_USER {
        section.push_str(&format!("user = \"{user}\"\n"));
    }
    section.push_str(&format!("host_key = \"{key}\"\n"));
    let kept = remove_node(text, name);
    let kept = kept.trim_end();
    Ok(if kept.is_empty() { section } else { format!("{kept}\n\n{section}") })
}

pub fn remove_node(text: &str, name: &str) -> String {
    let header = Regex::new(&format!(r#"^\s*\[\s*nodes\."?{}"?\s*]\s*(#.*)?$"#, regex::escape(name))).unwrap();
    let mut skipping = false;
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            if line.trim_start().starts_with('[') {
                skipping = header.is_match(line);
            }
            !skipping
        })
        .collect();
    let out = kept.join("\n");
    Regex::new(r"\n{3,}").unwrap().replace_all(&out, "\n\n").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::hub::HubConfig;

    #[test]
    fn sha256_and_hmac_vectors() {
        assert_eq!(hex(&Sha256::digest(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // RFC 4231, test cases 2 and 6.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn fingerprints_match_ssh_keygen() {
        assert_eq!(
            fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy test")
                .unwrap(),
            "SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU"
        );
        assert!(fingerprint("not a key").is_err());
        assert_eq!(without_comment(" ssh-ed25519 AAAA root@nas "), "ssh-ed25519 AAAA");
    }

    #[test]
    fn join_urls() {
        let line = "http://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU.zyxwvutsrqponmlkjihgfedcba";
        let url = JoinUrl::parse(line).unwrap();
        assert_eq!(url.base, "http://100.64.0.2:7341");
        assert_eq!(url.authority(), "100.64.0.2:7341");
        assert_eq!(url.code, "abcdefghijklmnopqrstuvwxyz");
        assert_eq!(url.secret, "zyxwvutsrqponmlkjihgfedcba");
        assert_eq!(url.to_string(), line);
        for bad in [
            "http://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU",
            "http://hub.lan:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU.zyxwvutsrqponmlkjihgfedcba",
            "https://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU.zyxwvutsrqponmlkjihgfedcba",
        ] {
            assert!(JoinUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_arrival_is_signed_over_every_field() {
        let a = Arrival::new("ssh-ed25519 AAAA", "limen-read", 22, Some("10.0.0.7")).signed("s");
        assert_eq!(a.proof, a.proof_with("s"));
        assert_ne!(a.proof, a.proof_with("t"));
        let mut moved = a.clone();
        moved.address = Some("10.0.0.66".into());
        assert_ne!(moved.proof_with("s"), a.proof);
    }

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy";

    #[test]
    fn hub_file_replaces_only_the_node() {
        let text = "# my hub\n[ssh]\nidentity = \"id_ed25519\"\n\n[nodes.nas]\nhost = \"10.0.0.1\"\nhost_key = \"ssh-rsa AAAA\"\n\n[nodes.router]\nhost = \"10.0.0.2\"\nhost_key = \"ssh-rsa BBBB\"\n";
        let updated = upsert_node(text, "nas", "10.0.0.9", 2222, "root", &format!("{KEY} root@nas")).unwrap();
        let config = HubConfig::parse(&updated).unwrap();
        assert!(updated.starts_with("# my hub\n"));
        let nas = config.node("nas").unwrap();
        assert_eq!(
            (nas.host.as_str(), nas.port, nas.user.as_str(), nas.host_key.as_str()),
            ("10.0.0.9", 2222, "root", KEY)
        );
        assert_eq!(config.node("router").unwrap().host, "10.0.0.2");
        assert!(!remove_node(&updated, "nas").contains("nodes.nas"));
    }

    #[test]
    fn hub_file_writes_nothing_but_its_own_values() {
        for (host, user, key) in [
            ("10.0.0.7\"\n[nodes.evil]\nhost = \"6.6.6.6", "limen-read", KEY.to_string()),
            ("10.0.0.7", "root\"\n[http]\n#", KEY.to_string()),
            ("10.0.0.7", "limen-read", format!("{KEY}\"\n[http]\nlisten = \"0.0.0.0:1")),
        ] {
            assert!(upsert_node("", "nas", host, 22, user, &key).is_err(), "{host} {user} {key}");
        }
    }
}
