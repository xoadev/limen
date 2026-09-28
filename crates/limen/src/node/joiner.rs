//! `limen join` (spec §10.1): this node joins a hub. With a join line, the hub's key is downloaded and checked against
//! the fingerprint in the line —what stops anyone between the two from slipping in their own key—, the node is
//! installed with it, and the hub is told this node's host key, signed with the line's secret. With `--hub-key` there
//! is no hub to talk to: the node is installed and prints the `limen trust` line for the hub.

use super::installer::{Installer, Outcome, RepoOptions, say};
use crate::os::http;
use limen_core::config::hub::{self, is};
use limen_core::join::{self, Arrival, Invitation, JoinUrl, Welcome};
use serde_json::Value;

pub struct Joiner {
    pub installer: Installer,
}

impl Joiner {
    pub fn join(
        &self,
        url: &JoinUrl,
        deploy_key: Option<&str>,
        from: Option<&str>,
        repo: Option<&dyn Fn(&str) -> RepoOptions>,
        address: Option<&str>,
        ssh_port: u16,
    ) -> Outcome<i32> {
        let invitation = fetch_invitation(url)?;
        let fingerprint = verify_invitation(url, &invitation)?;
        say(&format!("Joining the hub at {} as '{}' (hub key {fingerprint})", url.base, invitation.name));
        let repo = repo.map(|repo_for| repo_for(&invitation.name));
        self.installer.install(&invitation.hub_key, deploy_key, from, repo.as_ref(), false)?;
        let host_key = self.installer.host_key().ok_or("cannot read this machine's SSH host key")?;
        let arrival = Arrival::new(&join::without_comment(&host_key), &self.installer.read_user(), ssh_port, address)
            .signed(&url.secret);
        let welcome = arrive(url, &arrival)?;
        Ok(announce_welcome(&welcome, ssh_port))
    }

    pub fn with_key(
        &self,
        hub_key: &str,
        name: &str,
        deploy_key: Option<&str>,
        from: Option<&str>,
        repo: Option<&RepoOptions>,
        ssh_port: u16,
    ) -> Outcome<i32> {
        join::fingerprint(hub_key).map_err(|failure| format!("--hub-key: {}", failure.message))?;
        self.installer.install(hub_key, deploy_key, from, repo, false)?;
        let host_key = self
            .installer
            .host_key()
            .map(|key| join::without_comment(&key))
            .ok_or("cannot read this machine's SSH host key")?;
        let mut trust_options = String::new();
        if self.installer.read_user() != hub::READ_USER {
            trust_options.push_str(&format!(" --user {}", self.installer.read_user()));
        }
        if ssh_port != 22 {
            trust_options.push_str(&format!(" --port {ssh_port}"));
        }
        say("");
        say("limen is installed. On the hub, with this machine's address:");
        say(&format!("  limen trust {name} <address> '{host_key}'{trust_options}"));
        Ok(0)
    }
}

fn fetch_invitation(url: &JoinUrl) -> Outcome<Invitation> {
    let (status, body) = ask_hub(url, None)?;
    if status != 200 {
        return Err(hub_error(&body).unwrap_or(format!("the hub answered HTTP {status}")));
    }
    let invitation: Invitation = serde_json::from_str(&body).map_err(|_| "the hub's answer is not an invitation")?;
    // The name becomes the repository folder in this node's limen.toml: from a hub, it is checked like any input.
    if !is(hub::NODE_NAME, &invitation.name) {
        return Err(format!("the hub named this machine '{}', which is not a node name", invitation.name));
    }
    Ok(invitation)
}

/// The hub's key must have the join line's fingerprint, and the invitation its secret's signature. The fingerprint,
/// when both hold.
fn verify_invitation(url: &JoinUrl, invitation: &Invitation) -> Outcome<String> {
    let fingerprint = join::fingerprint(&invitation.hub_key).map_err(|failure| failure.message)?;
    if fingerprint != url.fingerprint {
        return Err(format!(
            "the hub's key ({fingerprint}) is not the one the join line names ({}): something between this machine and \
             the hub changed it. Nothing was installed.",
            url.fingerprint
        ));
    }
    if !invitation.is_signed_with(&url.secret) {
        return Err(
            "the hub's invitation is not signed with the join line's secret: something between this machine and \
             the hub changed it. Nothing was installed."
                .into(),
        );
    }
    Ok(fingerprint)
}

fn arrive(url: &JoinUrl, arrival: &Arrival) -> Outcome<Welcome> {
    let arrival = serde_json::to_string(arrival).expect("an arrival serializes");
    let (status, body) = ask_hub(url, Some(&arrival))?;
    if status != 200 {
        return Err(format!(
            "installed, but the hub refused it: {}",
            hub_error(&body).unwrap_or(format!("HTTP {status}"))
        ));
    }
    serde_json::from_str(&body).map_err(|_| "the hub's answer is not a welcome".into())
}

/// What the hub says of this node once it is on it. The exit code: 1 while the hub can't reach it.
fn announce_welcome(welcome: &Welcome, ssh_port: u16) -> i32 {
    let (name, address, detail) = (printable(&welcome.name), printable(&welcome.address), printable(&welcome.detail));
    say("");
    if welcome.reachable {
        say(&format!("{name} is on the hub, at {address}: {detail}."));
        return 0;
    }
    say(&format!("{name} is on the hub at {address}, but the hub can't reach it yet: {detail}"));
    say(&format!(
        "Check that the hub reaches this machine's SSH port ({ssh_port}) at that address, or join again with --address."
    ));
    1
}

/// One request to `/join/<code>`: GET without a body, POST with one. The status and the body, whatever the status.
fn ask_hub(url: &JoinUrl, body: Option<&str>) -> Outcome<(u16, String)> {
    let method = if body.is_some() { "POST" } else { "GET" };
    let response = http::request(url.authority(), method, &format!("/join/{}", url.code), body)
        .map_err(|message| format!("cannot reach the hub at {}: {message}", url.base))?;
    Ok((response.status, response.body))
}

fn hub_error(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok()?.get("error")?.as_str().map(printable)
}

/// What the hub says, without control characters: it goes to root's terminal, where they would be commands.
fn printable(text: &str) -> String {
    text.chars().filter(|character| !character.is_control()).collect()
}
