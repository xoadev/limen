//! `limen join` (spec §10.1): this node joins a hub. With a join line, the hub's key is downloaded and checked against
//! the fingerprint in the line —what stops anyone between the two from slipping in their own key—, the node is
//! installed with it, and the hub is told this node's host key, signed with the line's secret. With `--hub-key` there
//! is no hub to talk to: the node is installed and prints the `limen trust` line for the hub.

use super::installer::{Installer, Outcome, RepoOptions};
use crate::os::{http, sys};
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
        let invitation = fetch(url)?;
        let fingerprint = join::fingerprint(&invitation.hub_key).map_err(|e| e.message)?;
        if fingerprint != url.fingerprint {
            return Err(format!(
                "the hub's key ({fingerprint}) is not the one the join line names ({}): something between this machine and \
                 the hub changed it. Nothing was installed.",
                url.fingerprint
            ));
        }
        if !invitation.is_signed_with(&url.secret) {
            return Err(
                "the hub's invitation is not signed with the join line's secret: something between this machine \
                        and the hub changed it. Nothing was installed."
                    .into(),
            );
        }
        say(&format!("Joining the hub at {} as '{}' (hub key {fingerprint})", url.base, invitation.name));
        let repo = repo.map(|r| r(&invitation.name));
        self.installer.install(&invitation.hub_key, deploy_key, from, repo.as_ref(), false)?;
        let host_key = self.installer.host_key().ok_or("cannot read this machine's SSH host key")?;
        let arrival = Arrival::new(&join::without_comment(&host_key), &self.installer.read_user(), ssh_port, address)
            .signed(&url.secret);
        let welcome = arrive(url, &arrival)?;
        let (name, address, detail) =
            (printable(&welcome.name), printable(&welcome.address), printable(&welcome.detail));
        say("");
        if welcome.reachable {
            say(&format!("{name} is on the hub, at {address}: {detail}."));
            Ok(0)
        } else {
            say(&format!("{name} is on the hub at {address}, but the hub can't reach it yet: {detail}"));
            say(&format!(
                "Check that the hub reaches this machine's SSH port ({ssh_port}) at that address, or join again with --address."
            ));
            Ok(1)
        }
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
        join::fingerprint(hub_key).map_err(|e| format!("--hub-key: {}", e.message))?;
        self.installer.install(hub_key, deploy_key, from, repo, false)?;
        let host_key = self
            .installer
            .host_key()
            .map(|k| join::without_comment(&k))
            .ok_or("cannot read this machine's SSH host key")?;
        let mut extra = String::new();
        if self.installer.read_user() != hub::READ_USER {
            extra.push_str(&format!(" --user {}", self.installer.read_user()));
        }
        if ssh_port != 22 {
            extra.push_str(&format!(" --port {ssh_port}"));
        }
        say("");
        say("limen is installed. On the hub, with this machine's address:");
        say(&format!("  limen trust {name} <address> '{host_key}'{extra}"));
        Ok(0)
    }
}

fn fetch(url: &JoinUrl) -> Outcome<Invitation> {
    let (status, body) = http(url, None)?;
    if status != 200 {
        return Err(error_of(&body).unwrap_or(format!("the hub answered HTTP {status}")));
    }
    let invitation: Invitation = serde_json::from_str(&body).map_err(|_| "the hub's answer is not an invitation")?;
    // The name becomes the repository folder in this node's limen.toml: from a hub, it is checked like any input.
    if !is(hub::NODE_NAME, &invitation.name) {
        return Err(format!("the hub named this machine '{}', which is not a node name", invitation.name));
    }
    Ok(invitation)
}

fn arrive(url: &JoinUrl, arrival: &Arrival) -> Outcome<Welcome> {
    let (status, body) = http(url, Some(serde_json::to_string(arrival).expect("an arrival serializes")))?;
    if status != 200 {
        return Err(format!(
            "installed, but the hub refused it: {}",
            error_of(&body).unwrap_or(format!("HTTP {status}"))
        ));
    }
    serde_json::from_str(&body).map_err(|_| "the hub's answer is not a welcome".into())
}

/// One request to `/join/<code>`: GET without a body, POST with one. The status and the body, whatever the status.
fn http(url: &JoinUrl, body: Option<String>) -> Outcome<(u16, String)> {
    let method = if body.is_some() { "POST" } else { "GET" };
    let r = http::request(url.authority(), method, &format!("/join/{}", url.code), body.as_deref())
        .map_err(|e| format!("cannot reach the hub at {}: {e}", url.base))?;
    Ok((r.status, r.body))
}

fn error_of(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok()?.get("error")?.as_str().map(printable)
}

/// What the hub says, without control characters: it goes to root's terminal, where they would be commands.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn say(text: &str) {
    sys::out(&format!("{text}\n"));
}
