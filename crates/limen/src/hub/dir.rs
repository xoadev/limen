//! The hub is a directory (spec §7.2): its key, `limen.toml` with the nodes, `token` for HTTP clients and the pending
//! invitations. Everything that reads or changes it goes through here.

use super::ssh::SshClient;
use super::{NodeClient, constant_time_eq};
use crate::os::{fs, proc, sys};
use limen_core::config::hub::{self, HubConfig, is};
use limen_core::join::{self, Arrival, Invitation, PendingInvite, Welcome};
use limen_core::protocol::{ErrorCode, NodeResponse, Result, error};
use limen_core::time;
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Spec §9. The token `init` writes has 32.
pub const MIN_TOKEN: usize = 16;

/// The nodes go after `[ssh]` as they join, so what is commented out comes first.
const CONFIG_TEMPLATE: &str = r#"# limen hub (docs/spec.md §7.2). Nodes are added by `limen invite` and `limen trust`, or by hand.

# [http]
# Where nodes reach this hub to join: an address on your network or VPN, not a name.
# public_url = "http://100.64.0.2:7341"

[ssh]
identity = "id_ed25519"
"#;

/// An invitation as `limen invite` prints it: the code goes on the wire, the secret stays in the line's fragment.
pub struct IssuedInvite {
    pub code: String,
    pub secret: String,
}

pub struct Hub {
    pub home: String,
    pub config_path: String,
    pub key_path: String,
    pub token_path: String,
    invites: String,
    arrivals: Mutex<()>,
}

impl Hub {
    pub fn new(home: &str) -> Hub {
        let home = home.trim_end_matches('/').to_string();
        Hub {
            config_path: format!("{home}/limen.toml"),
            key_path: format!("{home}/id_ed25519"),
            token_path: format!("{home}/token"),
            invites: format!("{home}/invites"),
            home,
            arrivals: Mutex::new(()),
        }
    }

    /// `--home`, else `$LIMEN_HOME`, else `~/.limen`.
    pub fn home(option: Option<&str>) -> String {
        option
            .map(String::from)
            .or_else(|| sys::env("LIMEN_HOME").filter(|h| !h.trim().is_empty()))
            .unwrap_or_else(|| format!("{}/.limen", sys::env("HOME").unwrap_or("/root".into())))
    }

    pub fn at(option: Option<&str>) -> Hub {
        Hub::new(&Hub::home(option))
    }

    pub fn public_key(&self) -> Result<String> {
        fs::read_following(&format!("{}.pub", self.key_path))
            .map(|k| k.trim().to_string())
            .ok_or_else(|| error(ErrorCode::Unavailable, "no hub key; run `limen init`"))
    }

    pub fn config(&self) -> Result<HubConfig> {
        let text = fs::read_following(&self.config_path)
            .ok_or_else(|| error(ErrorCode::Unavailable, format!("no {}; run `limen init`", self.config_path)))?;
        let mut config =
            HubConfig::parse(&text).map_err(|e| error(ErrorCode::BadRequest, format!("{}: {e}", self.config_path)))?;
        if let Some(url) = sys::env("LIMEN_PUBLIC_URL").filter(|u| !u.trim().is_empty()) {
            let url = url.trim_end_matches('/').to_string();
            if !is(hub::PUBLIC_URL, &url) {
                return Err(error(
                    ErrorCode::BadRequest,
                    "LIMEN_PUBLIC_URL: expected http://<address>:<port>, an address and not a name",
                ));
            }
            config.public_url = Some(url);
        }
        Ok(config)
    }

    /// Creates what is missing and leaves what exists: the key pair, `limen.toml` and, for `serve`, the token of the
    /// HTTP clients. Returns what it created.
    pub fn init(&self, serve: bool) -> Result<Vec<String>> {
        let internal = |e: String| error(ErrorCode::Internal, e);
        let mut created = Vec::new();
        fs::mkdirs(&self.home, 0o700).map_err(internal)?;
        if !fs::exists(&self.key_path) {
            let keygen = proc::which("ssh-keygen")
                .ok_or_else(|| error(ErrorCode::Unavailable, "ssh-keygen is not installed"))?;
            let comment = format!("limen-hub@{}", sys::hostname());
            let argv: Vec<String> =
                [keygen.as_str(), "-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f", &self.key_path]
                    .map(String::from)
                    .to_vec();
            let r = proc::run(&argv, proc::Run::default()).map_err(internal)?;
            if r.exit_code != 0 {
                return Err(internal(format!("ssh-keygen: {}", r.err().trim())));
            }
            created.push(self.key_path.clone());
        }
        if !fs::exists(&self.config_path) {
            fs::write_atomic(&self.config_path, CONFIG_TEMPLATE.as_bytes(), 0o600).map_err(internal)?;
            created.push(self.config_path.clone());
        }
        if serve && !fs::exists(&self.token_path) && sys::env("LIMEN_TOKEN").is_none_or(|t| t.trim().is_empty()) {
            fs::write_atomic(&self.token_path, format!("{}\n", random(32)?).as_bytes(), 0o600).map_err(internal)?;
            created.push(self.token_path.clone());
        }
        Ok(created)
    }

    /// The HTTP clients' token: `LIMEN_TOKEN`, or the one `init` wrote.
    pub fn token(&self) -> Result<String> {
        let token = sys::env("LIMEN_TOKEN")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| fs::read_following(&self.token_path).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()))
            .ok_or_else(|| error(ErrorCode::Unavailable, "no token: set LIMEN_TOKEN or run `limen init --serve`"))?;
        if token.chars().count() < MIN_TOKEN {
            return Err(error(
                ErrorCode::BadRequest,
                format!(
                    "the token is {} characters long; it needs {MIN_TOKEN} characters or more",
                    token.chars().count()
                ),
            ));
        }
        Ok(token)
    }

    /// A one-time invitation for [name], valid for [ttl] (spec §10.1).
    pub fn invite(&self, name: &str, ttl: Duration) -> Result<IssuedInvite> {
        if !is(hub::NODE_NAME, name) {
            return Err(error(ErrorCode::BadRequest, format!("a node name matches {}", hub::NODE_NAME)));
        }
        fs::mkdirs(&self.invites, 0o700).map_err(|e| error(ErrorCode::Internal, e))?;
        let (code, secret) = (random(26)?, random(26)?);
        let pending = PendingInvite {
            name: name.into(),
            expires_epoch: time::now() + ttl.as_secs() as i64,
            secret: secret.clone(),
        };
        let text = serde_json::to_string(&pending).expect("an invitation serializes");
        fs::write_atomic(&format!("{}/{code}.json", self.invites), text.as_bytes(), 0o600)
            .map_err(|e| error(ErrorCode::Internal, e))?;
        Ok(IssuedInvite { code, secret })
    }

    pub fn pending(&self, code: &str) -> Option<PendingInvite> {
        if !is(join::CODE, code) {
            return None;
        }
        let path = format!("{}/{code}.json", self.invites);
        let invite: PendingInvite = serde_json::from_str(&fs::read_text(&path)?).ok()?;
        if invite.expires_epoch < time::now() {
            fs::remove(&path);
            return None;
        }
        Some(invite)
    }

    pub fn invitation(&self, code: &str) -> Option<Invitation> {
        let invite = self.pending(code)?;
        Some(Invitation::new(&invite.name, &self.public_key().ok()?).signed(&invite.secret))
    }

    /// A node that used [code] arrives: its entry goes into limen.toml, the invitation is spent, and the hub tries it
    /// at once, so the node's installer can say whether it worked.
    pub fn arrive(&self, code: &str, arrival: &Arrival, from: &str, client: &dyn NodeClient) -> Result<Welcome> {
        let (invite, address) = {
            let _one_at_a_time = self.arrivals.lock().unwrap();
            self.admit(code, arrival, from)?
        };
        let hello = client.call(&invite.name, "hello", &Map::new(), None);
        let detail = if hello.ok {
            let field = |k: &str| hello.data.as_ref().and_then(|d| d.get(k)).and_then(Value::as_str).map(String::from);
            format!("{}, limen {}", field("os").unwrap_or("Linux".into()), field("version").unwrap_or_default())
        } else {
            hello.error.as_ref().map(|e| format!("{}: {}", e.code, e.message)).unwrap_or_default()
        };
        Ok(Welcome { name: invite.name, address, reachable: hello.ok, detail })
    }

    /// Checks the arrival, writes the node and spends the invitation.
    fn admit(&self, code: &str, arrival: &Arrival, from: &str) -> Result<(PendingInvite, String)> {
        let invite = self.pending(code).ok_or_else(|| {
            error(ErrorCode::NotFound, "this invitation does not exist, was used, or expired; ask the hub for another")
        })?;
        // Refused without spending the invitation: a forged arrival must not take the real node's place, or its turn.
        if !constant_time_eq(&arrival.proof, &arrival.proof_with(&invite.secret)) {
            return Err(error(ErrorCode::BadRequest, "the arrival is not signed with the join line's secret"));
        }
        let address = arrival
            .address
            .clone()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| from.trim_start_matches("::ffff:").to_string());
        self.write_node(&invite.name, &address, arrival.port, &arrival.user, &arrival.host_key)?;
        fs::remove(&format!("{}/{code}.json", self.invites));
        Ok((invite, address))
    }

    /// Adds or replaces a node by hand (`limen trust`).
    pub fn trust(&self, name: &str, address: &str, host_key: &str, user: &str, port: u16) -> Result<()> {
        self.write_node(name, address, port, user, host_key)
    }

    /// Writes one node's table, checked by [join::upsert_node], and a file that still reads as a hub's.
    fn write_node(&self, name: &str, address: &str, port: u16, user: &str, host_key: &str) -> Result<()> {
        let text = fs::read_following(&self.config_path).unwrap_or_default();
        let updated = join::upsert_node(&text, name, address, port, user, host_key)?;
        HubConfig::parse(&updated).map_err(|e| {
            error(ErrorCode::BadRequest, format!("{} would not read after adding {name}: {e}", self.config_path))
        })?;
        fs::write_following(&self.config_path, updated.as_bytes(), 0o600).map_err(|e| error(ErrorCode::Internal, e))
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        let Some(text) = fs::read_following(&self.config_path) else { return Ok(false) };
        let Some(updated) = join::remove_node(&text, name)? else { return Ok(false) };
        fs::write_following(&self.config_path, updated.as_bytes(), 0o600).map_err(|e| error(ErrorCode::Internal, e))?;
        Ok(true)
    }
}

/// [length] characters of base32 from the kernel's random source: 5 bits each.
pub fn random(length: usize) -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = vec![0u8; length];
    let filled = rustix::rand::getrandom(&mut bytes, rustix::rand::GetRandomFlags::empty())
        .map_err(|e| error(ErrorCode::Internal, format!("no randomness: {e}")))?;
    if filled != length {
        return Err(error(ErrorCode::Internal, "the kernel gave less randomness than asked"));
    }
    Ok(bytes.iter().map(|b| ALPHABET[(*b % 32) as usize] as char).collect())
}

/// A [NodeClient] that follows limen.toml: the file is read on every call and the SSH client rebuilt when it changed,
/// so a node that joins is there for the next request, with no restart.
pub struct LiveHub {
    pub hub: Arc<Hub>,
    state: Mutex<Option<(String, Arc<SshClient>)>>,
}

impl LiveHub {
    pub fn new(hub: Arc<Hub>) -> Self {
        LiveHub { hub, state: Mutex::new(None) }
    }

    pub fn ssh(&self) -> Result<Arc<SshClient>> {
        let text = fs::read_following(&self.hub.config_path).unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        if let Some((seen, client)) = state.as_ref() {
            if *seen == text {
                return Ok(client.clone());
            }
        }
        let client = Arc::new(SshClient::new(self.hub.config()?, &self.hub.home)?);
        *state = Some((text, client.clone()));
        Ok(client)
    }

    pub fn config(&self) -> Result<HubConfig> {
        Ok(self.ssh()?.config().clone())
    }
}

impl NodeClient for LiveHub {
    fn nodes(&self) -> Result<Vec<String>> {
        self.ssh()?.nodes()
    }

    fn call(&self, node: &str, request: &str, args: &Map<String, Value>, timeout: Option<Duration>) -> NodeResponse {
        match self.ssh() {
            Ok(client) => client.call(node, request, args, timeout),
            Err(e) => NodeResponse::failure(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy";

    /// A hub in a directory of its own, removed at the end.
    struct Temp(Hub);

    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.0.home.rsplit_once('/').unwrap().0).ok();
        }
    }

    fn hub() -> Temp {
        let dir = std::env::temp_dir().join(format!("limen-hub-{}-{}", std::process::id(), random(8).unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = Hub::new(&format!("{}/hub", dir.display()));
        h.init(false).unwrap();
        Temp(h)
    }

    struct Answering;

    impl NodeClient for Answering {
        fn nodes(&self) -> Result<Vec<String>> {
            Ok(vec!["nas".into()])
        }

        fn call(&self, _: &str, _: &str, _: &Map<String, Value>, _: Option<Duration>) -> NodeResponse {
            NodeResponse::success(json!({"os": "Debian GNU/Linux 13", "version": "0.1.0"}), false)
        }
    }

    #[test]
    fn init_creates_what_is_missing_only() {
        let dir = std::env::temp_dir().join(format!("limen-init-{}", random(8).unwrap()));
        let h = Hub::new(&format!("{}/hub", dir.display()));
        let created = h.init(true).unwrap();
        assert_eq!(created, [h.key_path.clone(), h.config_path.clone(), h.token_path.clone()]);
        assert!(join::fingerprint(&h.public_key().unwrap()).unwrap().starts_with("SHA256:"));
        assert_eq!(h.token().unwrap().len(), 32);
        assert!(h.init(true).unwrap().is_empty());
        assert!(h.config().unwrap().nodes.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_invitation_joins_once_with_the_address_it_came_from() {
        let t = hub();
        let issued = t.0.invite("nas", Duration::from_secs(3600)).unwrap();
        assert_eq!(t.0.invitation(&issued.code).unwrap().name, "nas");
        let host_key = format!("{KEY} root@nas");
        let arrival = Arrival::new(&host_key, "limen-read", 22, None).signed(&issued.secret);
        let welcome = t.0.arrive(&issued.code, &arrival, "::ffff:10.0.0.7", &Answering).unwrap();
        assert!(welcome.reachable);
        assert_eq!(welcome.address, "10.0.0.7");
        assert_eq!(welcome.detail, "Debian GNU/Linux 13, limen 0.1.0");
        let node = t.0.config().unwrap().node("nas").unwrap().clone();
        assert_eq!(node.host, "10.0.0.7");
        assert_eq!(node.host_key, KEY);
        assert!(t.0.invitation(&issued.code).is_none());
        assert!(t.0.arrive(&issued.code, &arrival, "10.0.0.8", &Answering).is_err());
    }

    #[test]
    fn an_arrival_without_the_secret_takes_nobodys_place() {
        // Whoever sees the code on the wire arrives first, with their own machine: refused, and the invitation waits.
        let t = hub();
        let issued = t.0.invite("nas", Duration::from_secs(3600)).unwrap();
        for forged in [
            Arrival::new(KEY, "limen-read", 22, None),
            Arrival::new(KEY, "limen-read", 22, None).signed(&"a".repeat(26)),
        ] {
            assert!(t.0.arrive(&issued.code, &forged, "10.0.0.66", &Answering).is_err());
        }
        // Nor can a real arrival be changed on the way: the proof covers every field.
        let real = Arrival::new(KEY, "limen-read", 22, Some("10.0.0.7")).signed(&issued.secret);
        let mut moved = real.clone();
        moved.address = Some("10.0.0.66".into());
        assert!(t.0.arrive(&issued.code, &moved, "10.0.0.66", &Answering).is_err());
        assert!(t.0.config().unwrap().nodes.is_empty());
        assert!(t.0.invitation(&issued.code).is_some());
        t.0.arrive(&issued.code, &real, "10.0.0.7", &Answering).unwrap();
        assert_eq!(t.0.config().unwrap().node("nas").unwrap().host, "10.0.0.7");
    }

    #[test]
    fn invitations_expire_and_codes_are_checked() {
        let t = hub();
        let issued = t.0.invite("router", Duration::ZERO).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(t.0.invitation(&issued.code).is_none());
        assert!(t.0.invitation("../../etc/passwd").is_none());
        assert!(t.0.invite("Not A Name", Duration::from_secs(60)).is_err());
    }

    #[test]
    fn an_arrival_can_only_add_the_node_it_was_invited_as() {
        let t = hub();
        let before = std::fs::read_to_string(&t.0.config_path).unwrap();
        let injections = [
            Arrival::new(
                KEY,
                "limen-read",
                22,
                Some(&format!("10.0.0.7\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"{KEY}")),
            ),
            Arrival::new(KEY, "limen-read", 22, Some("10.0.0.7\"\n[http]\norigins = [\"http://evil\"]\n#")),
            Arrival::new(
                KEY,
                &format!("root\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"{KEY}\"\n#"),
                22,
                Some("10.0.0.7"),
            ),
            Arrival::new(&format!("{KEY}\"\n[http]\nlisten = \"0.0.0.0:1\"\n#"), "limen-read", 22, Some("10.0.0.7")),
            // Well formed: the hub's own `host_key` line completes the injected node.
            Arrival::new(
                KEY,
                "limen-read",
                22,
                Some(&format!("10.0.0.7\"\nhost_key = \"{KEY}\"\n[nodes.evil]\nhost = \"6.6.6.6")),
            ),
        ];
        for arrival in injections {
            // Signed: what is tested is the hub's check of the values, not the proof.
            let issued = t.0.invite("nas", Duration::from_secs(60)).unwrap();
            assert!(
                t.0.arrive(&issued.code, &arrival.clone().signed(&issued.secret), "10.0.0.7", &Answering).is_err(),
                "{arrival:?}"
            );
            assert_eq!(std::fs::read_to_string(&t.0.config_path).unwrap(), before);
        }
    }

    #[test]
    fn a_short_token_is_refused() {
        let t = hub();
        std::fs::write(&t.0.token_path, "short\n").unwrap();
        assert_eq!(t.0.token().unwrap_err().code, ErrorCode::BadRequest);
        std::fs::write(&t.0.token_path, format!("{}\n", "a".repeat(MIN_TOKEN))).unwrap();
        assert_eq!(t.0.token().unwrap(), "a".repeat(MIN_TOKEN));
    }
}
