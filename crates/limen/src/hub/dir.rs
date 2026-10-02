//! The hub is a directory (spec §7.2): its key, `limen.toml` with the nodes, `token` for HTTP clients and the pending
//! invitations. Everything that reads or changes it goes through here.

use super::ssh::SshClient;
use super::{NodeClient, constant_time_eq, internal};
use crate::os::{fs, proc, sys};
use limen_core::config::hub::{self, Approval, HubConfig, is};
use limen_core::join::{self, Arrival, Invitation, PendingInvite, Welcome};
use limen_core::protocol::{ErrorCode, NodeError, NodeResponse, Result, error};
use limen_core::time;
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Spec §9. The token `init` writes has [TOKEN_LENGTH].
pub const MIN_TOKEN: usize = 16;
const TOKEN_LENGTH: usize = 32;
/// An invitation's code, as [join::CODE] has it, and its secret.
const INVITE_CODE_LENGTH: usize = 26;

/// The nodes go after `[ssh]` as they join, so what is commented out comes first.
const CONFIG_TEMPLATE: &str = r#"# limen hub (docs/spec.md §7.2). Nodes are added by `limen invite` and `limen trust`, or by hand.

# [http]
# Where nodes reach this hub to join: an address on your network or VPN, not a name.
# public_url = "http://100.64.0.2:7341"

[ssh]
# The hub's SSH key, under this directory unless absolute; `limen init` creates it when it is missing. Its public
# key, the one nodes trust, is the same path with `.pub`.
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
    pub token_path: String,
    invites_dir: String,
    arrivals: Mutex<()>,
}

impl Hub {
    pub fn new(home: &str) -> Hub {
        let home = home.trim_end_matches('/').to_string();
        Hub {
            config_path: format!("{home}/limen.toml"),
            token_path: format!("{home}/token"),
            invites_dir: format!("{home}/invites"),
            home,
            arrivals: Mutex::new(()),
        }
    }

    /// `--home`, else `$LIMEN_HOME`, else `~/.limen`.
    pub fn home(option: Option<&str>) -> String {
        option
            .map(String::from)
            .or_else(|| sys::env_setting("LIMEN_HOME"))
            .unwrap_or_else(|| format!("{}/.limen", sys::env("HOME").unwrap_or("/root".into())))
    }

    pub fn at(option: Option<&str>) -> Hub {
        Hub::new(&Hub::home(option))
    }

    /// The hub's SSH key, `[ssh].identity`: what `ssh -i` gets, and with `.pub` what nodes trust.
    pub fn key_path(&self) -> Result<String> {
        Ok(self.config()?.identity_path(&self.home))
    }

    pub fn public_key(&self) -> Result<String> {
        let public = format!("{}.pub", self.key_path()?);
        fs::read_following(&public).map(|key| key.trim().to_string()).ok_or_else(|| {
            error(
                ErrorCode::Unavailable,
                format!("no hub key at {public} ([ssh].identity in {}); run `limen init`", self.config_path),
            )
        })
    }

    pub fn config(&self) -> Result<HubConfig> {
        let text = fs::read_following(&self.config_path)
            .ok_or_else(|| error(ErrorCode::Unavailable, format!("no {}; run `limen init`", self.config_path)))?;
        let mut config = HubConfig::parse(&text)
            .map_err(|cause| error(ErrorCode::BadRequest, format!("{}: {cause}", self.config_path)))?;
        if let Some(url) = public_url_from_env()? {
            config.public_url = Some(url);
        }
        Ok(config)
    }

    /// Creates what is missing and leaves what exists: `limen.toml`, the key pair it names and, for `serve`, the token
    /// of the HTTP clients. Returns what it created.
    pub fn init(&self, serve: bool) -> Result<Vec<String>> {
        let mut created = Vec::new();
        fs::mkdirs(&self.home, 0o700).map_err(internal)?;
        if !fs::exists(&self.config_path) {
            fs::write_atomic(&self.config_path, CONFIG_TEMPLATE.as_bytes(), 0o600).map_err(internal)?;
            created.push(self.config_path.clone());
        }
        let key_path = self.key_path()?;
        if !fs::exists(&key_path) {
            generate_key(&key_path)?;
            created.push(key_path);
        }
        if serve && !fs::exists(&self.token_path) && sys::env_setting("LIMEN_TOKEN").is_none() {
            let token = format!("{}\n", random(TOKEN_LENGTH)?);
            fs::write_atomic(&self.token_path, token.as_bytes(), 0o600).map_err(internal)?;
            created.push(self.token_path.clone());
        }
        Ok(created)
    }

    /// The HTTP clients' token: `LIMEN_TOKEN`, or the one `init` wrote.
    pub fn token(&self) -> Result<String> {
        let token = sys::env_setting("LIMEN_TOKEN")
            .or_else(|| trimmed(fs::read_following(&self.token_path)))
            .ok_or_else(|| error(ErrorCode::Unavailable, "no token: set LIMEN_TOKEN or run `limen init --serve`"))?;
        let length = token.chars().count();
        if length < MIN_TOKEN {
            return Err(error(
                ErrorCode::BadRequest,
                format!("the token is {length} characters long; it needs {MIN_TOKEN} characters or more"),
            ));
        }
        Ok(token)
    }

    /// A one-time invitation for [name], valid for [ttl] (spec §10.1).
    pub fn invite(&self, name: &str, ttl: Duration) -> Result<IssuedInvite> {
        if !is(hub::NODE_NAME, name) {
            return Err(error(ErrorCode::BadRequest, format!("a node name matches {}", hub::NODE_NAME)));
        }
        fs::mkdirs(&self.invites_dir, 0o700).map_err(internal)?;
        let (code, secret) = (random(INVITE_CODE_LENGTH)?, random(INVITE_CODE_LENGTH)?);
        let pending = PendingInvite {
            name: name.into(),
            expires_epoch: time::now() + ttl.as_secs() as i64,
            secret: secret.clone(),
        };
        let text = serde_json::to_string(&pending).expect("an invitation serializes");
        fs::write_atomic(&self.invite_path(&code), text.as_bytes(), 0o600).map_err(internal)?;
        Ok(IssuedInvite { code, secret })
    }

    pub fn pending(&self, code: &str) -> Option<PendingInvite> {
        if !is(join::CODE, code) {
            return None;
        }
        let path = self.invite_path(code);
        let invite: PendingInvite = serde_json::from_str(&fs::read_text(&path)?).ok()?;
        if invite.expires_epoch < time::now() {
            fs::remove(&path);
            return None;
        }
        Some(invite)
    }

    /// Where the invitation of [code] waits. [code] is checked against [join::CODE] before it is used from outside.
    fn invite_path(&self, code: &str) -> String {
        format!("{}/{code}.json", self.invites_dir)
    }

    pub fn invitation(&self, code: &str) -> Option<Invitation> {
        let invite = self.pending(code)?;
        Some(Invitation::new(&invite.name, &self.public_key().ok()?).signed(&invite.secret))
    }

    /// A node that used [code] arrives: its entry goes into limen.toml, the invitation is spent, and the hub tries it
    /// at once, so the node's installer can say whether it worked.
    pub fn arrive(&self, code: &str, arrival: &Arrival, from: &str, client: &dyn NodeClient) -> Result<Welcome> {
        let (invite, address) = {
            let _one_at_a_time = self.arrivals.lock().expect("nothing panics holding the arrivals");
            self.admit(code, arrival, from)?
        };
        let hello = client.call(&invite.name, "hello", &Map::new(), None);
        Ok(Welcome { name: invite.name, address, reachable: hello.ok, detail: hello_detail(&hello) })
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
            .filter(|address| !address.trim().is_empty())
            .unwrap_or_else(|| from.trim_start_matches("::ffff:").to_string());
        self.write_node(&invite.name, &address, arrival.port, &arrival.user, &arrival.host_key)?;
        fs::remove(&self.invite_path(code));
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
        HubConfig::parse(&updated).map_err(|cause| {
            error(ErrorCode::BadRequest, format!("{} would not read after adding {name}: {cause}", self.config_path))
        })?;
        self.write_config(&updated)
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        let Some(text) = fs::read_following(&self.config_path) else { return Ok(false) };
        let Some(updated) = join::remove_node(&text, name)? else { return Ok(false) };
        self.write_config(&updated)?;
        Ok(true)
    }

    fn write_config(&self, text: &str) -> Result<()> {
        fs::write_following(&self.config_path, text.as_bytes(), 0o600).map_err(internal)
    }
}

/// `LIMEN_PUBLIC_URL`, which overrides `[http].public_url`.
fn public_url_from_env() -> Result<Option<String>> {
    let Some(url) = sys::env_setting("LIMEN_PUBLIC_URL") else { return Ok(None) };
    let url = url.trim_end_matches('/').to_string();
    if !is(hub::PUBLIC_URL, &url) {
        return Err(error(
            ErrorCode::BadRequest,
            "LIMEN_PUBLIC_URL: expected http://<address>:<port>, an address and not a name",
        ));
    }
    Ok(Some(url))
}

/// An ed25519 key pair at [path] and [path]`.pub`, without a passphrase: ssh runs in batch mode.
fn generate_key(path: &str) -> Result<()> {
    let keygen =
        proc::which("ssh-keygen").ok_or_else(|| error(ErrorCode::Unavailable, "ssh-keygen is not installed"))?;
    let comment = format!("limen-hub@{}", sys::hostname());
    let argv = [keygen.as_str(), "-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f", path].map(String::from);
    let result = proc::run(&argv, proc::Run::default()).map_err(internal)?;
    if result.exit_code != 0 {
        return Err(internal(format!("ssh-keygen: {}", result.err().trim())));
    }
    Ok(())
}

/// [text] without the whitespace around it, unless nothing is left.
fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// What the installer of a joining node is told of the hub's first request to it: the node's OS and limen version,
/// or why it did not answer.
fn hello_detail(hello: &NodeResponse) -> String {
    if !hello.ok {
        return hello.error.as_ref().map(NodeError::summary).unwrap_or_default();
    }
    let field = |name: &str| hello.data.as_ref().and_then(|data| data.get(name)).and_then(Value::as_str);
    format!("{}, limen {}", field("os").unwrap_or("Linux"), field("version").unwrap_or_default())
}

/// [length] characters of base32 from the kernel's random source: 5 bits each.
pub fn random(length: usize) -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = vec![0u8; length];
    let filled = rustix::rand::getrandom(&mut bytes, rustix::rand::GetRandomFlags::empty())
        .map_err(|cause| internal(format!("no randomness: {cause}")))?;
    if filled != length {
        return Err(internal("the kernel gave less randomness than asked"));
    }
    Ok(bytes.iter().map(|byte| ALPHABET[(*byte % 32) as usize] as char).collect())
}

/// A [NodeClient] that follows limen.toml: the file is read on every call and the SSH client rebuilt when it changed,
/// so a node that joins is there for the next request, with no restart.
pub struct LiveHub {
    pub hub: Arc<Hub>,
    /// The client built from limen.toml as it read then.
    built: Mutex<Option<(String, Arc<SshClient>)>>,
}

impl LiveHub {
    pub fn new(hub: Arc<Hub>) -> Self {
        LiveHub { hub, built: Mutex::new(None) }
    }

    pub fn ssh(&self) -> Result<Arc<SshClient>> {
        let text = fs::read_following(&self.hub.config_path).unwrap_or_default();
        let mut built = self.built.lock().expect("nothing panics holding the SSH client");
        if let Some((_, client)) = built.as_ref().filter(|(built_from, _)| *built_from == text) {
            return Ok(client.clone());
        }
        let client = Arc::new(SshClient::new(self.hub.config()?, &self.hub.home)?);
        *built = Some((text, client.clone()));
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
            Err(failure) => NodeResponse::failure(&failure),
        }
    }

    fn approval(&self) -> Result<Approval> {
        self.ssh()?.approval()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy";

    /// A hub in a directory of its own, removed at the end.
    struct TempHub(Hub);

    impl TempHub {
        fn uninitialised() -> Self {
            let dir = std::env::temp_dir().join(format!("limen-hub-{}-{}", std::process::id(), random(8).unwrap()));
            std::fs::create_dir_all(&dir).unwrap();
            TempHub(Hub::new(&format!("{}/hub", dir.display())))
        }

        fn initialised() -> Self {
            let temp = Self::uninitialised();
            temp.init(false).unwrap();
            temp
        }
    }

    impl std::ops::Deref for TempHub {
        type Target = Hub;

        fn deref(&self) -> &Hub {
            &self.0
        }
    }

    impl Drop for TempHub {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.home.rsplit_once('/').unwrap().0).ok();
        }
    }

    struct Answering;

    impl NodeClient for Answering {
        fn nodes(&self) -> Result<Vec<String>> {
            Ok(vec!["nas".into()])
        }

        fn call(&self, _: &str, _: &str, _: &Map<String, Value>, _: Option<Duration>) -> NodeResponse {
            NodeResponse::success(json!({"os": "Debian GNU/Linux 13", "version": "0.1.0"}), false)
        }

        fn approval(&self) -> Result<Approval> {
            Ok(Approval::default())
        }
    }

    #[test]
    fn init_creates_what_is_missing_only() {
        let hub = TempHub::uninitialised();
        let created = hub.init(true).unwrap();
        assert_eq!(created, [hub.config_path.clone(), format!("{}/id_ed25519", hub.home), hub.token_path.clone()]);
        assert!(join::fingerprint(&hub.public_key().unwrap()).unwrap().starts_with("SHA256:"));
        assert_eq!(hub.token().unwrap().len(), 32);
        assert!(hub.init(true).unwrap().is_empty());
        assert!(hub.config().unwrap().nodes.is_empty());
    }

    #[test]
    fn the_key_is_where_ssh_identity_says() {
        let hub = TempHub::uninitialised();
        let elsewhere = format!("{}/keys", hub.home.rsplit_once('/').unwrap().0);
        std::fs::create_dir_all(&elsewhere).unwrap();
        for (identity, key) in [
            ("hub_key".to_string(), format!("{}/hub_key", hub.home)),
            (format!("{elsewhere}/hub"), format!("{elsewhere}/hub")),
        ] {
            std::fs::create_dir_all(&hub.home).unwrap();
            std::fs::write(&hub.config_path, format!("[ssh]\nidentity = \"{identity}\"\n")).unwrap();
            assert_eq!(hub.init(false).unwrap(), std::slice::from_ref(&key));
            assert!(!std::path::Path::new(&format!("{}/id_ed25519", hub.home)).exists());
            let public = std::fs::read_to_string(format!("{key}.pub")).unwrap();
            assert_eq!(hub.public_key().unwrap(), public.trim());
        }
    }

    #[test]
    fn an_invitation_joins_once_with_the_address_it_came_from() {
        let hub = TempHub::initialised();
        let issued = hub.invite("nas", Duration::from_secs(3600)).unwrap();
        assert_eq!(hub.invitation(&issued.code).unwrap().name, "nas");
        let host_key = format!("{KEY} root@nas");
        let arrival = Arrival::new(&host_key, "limen", 22, None).signed(&issued.secret);
        let welcome = hub.arrive(&issued.code, &arrival, "::ffff:10.0.0.7", &Answering).unwrap();
        assert!(welcome.reachable);
        assert_eq!(welcome.address, "10.0.0.7");
        assert_eq!(welcome.detail, "Debian GNU/Linux 13, limen 0.1.0");
        let node = hub.config().unwrap().node("nas").unwrap().clone();
        assert_eq!(node.host, "10.0.0.7");
        assert_eq!(node.host_key, KEY);
        assert!(hub.invitation(&issued.code).is_none());
        assert!(hub.arrive(&issued.code, &arrival, "10.0.0.8", &Answering).is_err());
    }

    #[test]
    fn an_arrival_without_the_secret_takes_nobodys_place() {
        // Whoever sees the code on the wire arrives first, with their own machine: refused, and the invitation waits.
        let hub = TempHub::initialised();
        let issued = hub.invite("nas", Duration::from_secs(3600)).unwrap();
        for forged in
            [Arrival::new(KEY, "limen", 22, None), Arrival::new(KEY, "limen", 22, None).signed(&"a".repeat(26))]
        {
            assert!(hub.arrive(&issued.code, &forged, "10.0.0.66", &Answering).is_err());
        }
        // Nor can a real arrival be changed on the way: the proof covers every field.
        let real = Arrival::new(KEY, "limen", 22, Some("10.0.0.7")).signed(&issued.secret);
        let mut moved = real.clone();
        moved.address = Some("10.0.0.66".into());
        assert!(hub.arrive(&issued.code, &moved, "10.0.0.66", &Answering).is_err());
        assert!(hub.config().unwrap().nodes.is_empty());
        assert!(hub.invitation(&issued.code).is_some());
        hub.arrive(&issued.code, &real, "10.0.0.7", &Answering).unwrap();
        assert_eq!(hub.config().unwrap().node("nas").unwrap().host, "10.0.0.7");
    }

    #[test]
    fn invitations_expire_and_codes_are_checked() {
        let hub = TempHub::initialised();
        let issued = hub.invite("router", Duration::ZERO).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(hub.invitation(&issued.code).is_none());
        assert!(hub.invitation("../../etc/passwd").is_none());
        assert!(hub.invite("Not A Name", Duration::from_secs(60)).is_err());
    }

    #[test]
    fn an_arrival_can_only_add_the_node_it_was_invited_as() {
        let hub = TempHub::initialised();
        let before = std::fs::read_to_string(&hub.config_path).unwrap();
        let injections = [
            Arrival::new(
                KEY,
                "limen",
                22,
                Some(&format!("10.0.0.7\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"{KEY}")),
            ),
            Arrival::new(KEY, "limen", 22, Some("10.0.0.7\"\n[http]\norigins = [\"http://evil\"]\n#")),
            Arrival::new(
                KEY,
                &format!("root\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"{KEY}\"\n#"),
                22,
                Some("10.0.0.7"),
            ),
            Arrival::new(&format!("{KEY}\"\n[http]\nlisten = \"0.0.0.0:1\"\n#"), "limen", 22, Some("10.0.0.7")),
            // Well formed: the hub's own `host_key` line completes the injected node.
            Arrival::new(
                KEY,
                "limen",
                22,
                Some(&format!("10.0.0.7\"\nhost_key = \"{KEY}\"\n[nodes.evil]\nhost = \"6.6.6.6")),
            ),
        ];
        for arrival in injections {
            // Signed: what is tested is the hub's check of the values, not the proof.
            let issued = hub.invite("nas", Duration::from_secs(60)).unwrap();
            assert!(
                hub.arrive(&issued.code, &arrival.clone().signed(&issued.secret), "10.0.0.7", &Answering).is_err(),
                "{arrival:?}"
            );
            assert_eq!(std::fs::read_to_string(&hub.config_path).unwrap(), before);
        }
    }

    #[test]
    fn a_short_token_is_refused() {
        let hub = TempHub::initialised();
        std::fs::write(&hub.token_path, "short\n").unwrap();
        assert_eq!(hub.token().unwrap_err().code, ErrorCode::BadRequest);
        std::fs::write(&hub.token_path, format!("{}\n", "a".repeat(MIN_TOKEN))).unwrap();
        assert_eq!(hub.token().unwrap(), "a".repeat(MIN_TOKEN));
    }
}
