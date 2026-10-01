//! `limen join` (spec §10.1): this node joins a hub. With a join line, the hub's key is downloaded and checked against
//! the fingerprint in the line —what stops anyone between the two from slipping in their own key—, the node is
//! installed with it, and the hub is told this node's host key, signed with the line's secret. With `--hub-key` there
//! is no hub to talk to: the node is installed and prints the `limen trust` line for the hub.

use super::installer::{Installer, Outcome};
use crate::os::{http, sys};
use limen_core::config::hub::{self, is};
use limen_core::join::{self, Arrival, Invitation, JoinUrl, Welcome};
use serde_json::Value;

pub struct Joiner {
    pub installer: Installer,
}

impl Joiner {
    pub fn join(&self, url: &JoinUrl, from: Option<&str>, address: Option<&str>, ssh_port: u16) -> Outcome<i32> {
        let invitation = fetch_invitation(url)?;
        let fingerprint = verify_invitation(url, &invitation)?;
        sys::say(&format!("Joining the hub at {} as '{}' (hub key {fingerprint})", url.base, invitation.name));
        self.installer.install(Some(&invitation.hub_key), from, false)?;
        let host_key = self.installer.host_key().ok_or("cannot read this machine's SSH host key")?;
        let arrival = Arrival::new(&join::without_comment(&host_key), &self.installer.node_user(), ssh_port, address)
            .signed(&url.secret);
        let welcome = arrive(url, &arrival)?;
        Ok(announce_welcome(&welcome, ssh_port))
    }

    /// Installs with [hub_key] and prints the `limen trust` line for the hub, with [address] when it is given.
    pub fn with_key(
        &self,
        hub_key: &str,
        name: &str,
        from: Option<&str>,
        address: Option<&str>,
        ssh_port: u16,
    ) -> Outcome<i32> {
        join::fingerprint(hub_key).map_err(|failure| format!("--hub-key: {}", failure.message))?;
        if !is(hub::NODE_NAME, name) {
            return Err(format!("--name '{name}' is not a node name"));
        }
        // Checked as the hub checks a node's host: the line is pasted into a shell there.
        if let Some(address) = address.filter(|address| !is(hub::HOST, address)) {
            return Err(format!("--address '{address}' is not a host name or address"));
        }
        self.installer.install(Some(hub_key), from, false)?;
        let host_key = self
            .installer
            .host_key()
            .map(|key| join::without_comment(&key))
            .ok_or("cannot read this machine's SSH host key")?;
        sys::say("");
        sys::say(if address.is_some() {
            "limen is installed. On the hub:"
        } else {
            "limen is installed. On the hub, with this machine's address:"
        });
        let line = trust_line(name, address, &host_key, &self.installer.node_user(), ssh_port);
        sys::say(&format!("  {line}"));
        Ok(0)
    }
}

/// The line that adds this machine to the hub by hand: [address], or a placeholder for it.
fn trust_line(name: &str, address: Option<&str>, host_key: &str, user: &str, ssh_port: u16) -> String {
    let mut line = format!("limen trust {name} {} '{host_key}'", address.unwrap_or("<address>"));
    if user != hub::NODE_USER {
        line.push_str(&format!(" --user {user}"));
    }
    if ssh_port != 22 {
        line.push_str(&format!(" --port {ssh_port}"));
    }
    line
}

fn fetch_invitation(url: &JoinUrl) -> Outcome<Invitation> {
    let (status, body) = ask_hub(url, None)?;
    if status != 200 {
        return Err(hub_error(&body).unwrap_or(format!("the hub answered HTTP {status}")));
    }
    let invitation: Invitation = serde_json::from_str(&body).map_err(|_| "the hub's answer is not an invitation")?;
    // What this machine prints, and what the hub files it under: from a hub, it is checked like any input.
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
    sys::say("");
    if welcome.reachable {
        sys::say(&format!("{name} is on the hub, at {address}: {detail}."));
        return 0;
    }
    sys::say(&format!("{name} is on the hub at {address}, but the hub can't reach it yet: {detail}"));
    sys::say(&format!(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trust_line_has_the_address_when_it_is_given() {
        let key = "ssh-ed25519 AAAA";
        assert_eq!(trust_line("nas", None, key, "limen", 22), "limen trust nas <address> 'ssh-ed25519 AAAA'");
        assert_eq!(
            trust_line("router", Some("10.0.0.1"), key, "root", 2222),
            "limen trust router 10.0.0.1 'ssh-ed25519 AAAA' --user root --port 2222"
        );
    }
}
