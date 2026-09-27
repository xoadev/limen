//! `state` (spec §6.1): the repository commit on the node against the remote, and every service `node.toml` expects
//! against what runs. What `apply` checks at the end, and what an agent asks first after a deploy.

use super::system::{self, Init, ProcdState};
use super::{Answer, Node, repo};
use crate::os::{fs, proc};
use limen_core::config::expectations::Expectations;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::system::parsers;
use serde_json::{Map, Value, json};
use std::time::Duration;

/// One expected service and what the node says about it.
pub struct ServiceState {
    pub kind: &'static str,
    pub name: String,
    pub running: bool,
    pub detail: Map<String, Value>,
}

pub fn answer(node: &Node) -> Answer {
    let mut problems: Vec<String> = Vec::new();
    let repo_json = match &node.config.repo {
        None => Value::Null,
        Some(repo) => {
            let deployed = repo::deployed(repo);
            let remote = repo::remote(repo);
            let mut o = json!({
                "url": repo.display_url(),
                "branch": repo.branch,
                "path": repo.path,
                "deployed": deployed.as_ref().map(|c| json!({"commit": c.hash, "date": c.date, "subject": c.subject})),
            });
            match &remote {
                Ok(r) => {
                    o["remote"] = json!(r);
                    o["up_to_date"] = json!(deployed.as_ref().map(|c| &c.hash) == Some(r));
                }
                Err(e) => o["remote_error"] = json!(node.redactor.redact(&e.message)),
            }
            // git's own words, which can hold a URL with its credentials.
            o["last_sync"] = repo::last_sync(repo)
                .and_then(|s| serde_json::from_str(&node.redactor.redact(&s.to_string())).ok())
                .unwrap_or(Value::Null);
            match (&deployed, &remote) {
                (None, _) => problems.push("the repository is not checked out: run sync or apply".into()),
                (Some(d), Ok(r)) if &d.hash != r => problems.push(format!("the node is behind {}", repo.branch)),
                _ => {}
            }
            o
        }
    };
    let services = services(node).unwrap_or_else(|e| {
        problems.push(e.message);
        vec![]
    });
    for s in services.iter().filter(|s| !s.running) {
        problems.push(format!("{} {} is not running", s.kind, s.name));
    }
    let services: Vec<Value> = services
        .into_iter()
        .map(|s| {
            let mut o = Map::new();
            o.insert("kind".into(), json!(s.kind));
            o.insert("name".into(), json!(s.name));
            o.insert("running".into(), json!(s.running));
            o.extend(s.detail);
            Value::Object(o)
        })
        .collect();
    Answer::of(json!({"repo": repo_json, "services": services, "problems": problems}))
}

pub fn expectations(node: &Node) -> Result<Expectations> {
    let Some(path) = node.config.expectations() else { return Ok(Expectations::default()) };
    let Some(text) = fs::read_text(&path) else { return Ok(Expectations::default()) };
    Expectations::parse(&text).map_err(|e| error(ErrorCode::Internal, format!("{path}: {e}")))
}

pub fn services(node: &Node) -> Result<Vec<ServiceState>> {
    let expected = expectations(node)?;
    let mut out: Vec<ServiceState> = expected.compose.iter().map(|n| compose(node, n)).collect();
    out.extend(expected.units.iter().map(|n| unit(node, n)));
    out.extend(expected.procd.iter().map(|n| procd(node, n)));
    Ok(out)
}

/// `stacks/<name>/compose.yaml` of the node's folder: the file limen brings up and then asks about.
pub fn compose_file(node: &Node, name: &str) -> Result<String> {
    let dir = node.config.stacks().ok_or_else(|| error(ErrorCode::Unavailable, "stacks need [repo] in limen.toml"))?;
    ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"]
        .iter()
        .map(|f| format!("{dir}/{name}/{f}"))
        .find(|p| fs::exists(p))
        .ok_or_else(|| error(ErrorCode::NotFound, format!("no compose file in {dir}/{name}")))
}

fn missing(kind: &'static str, name: &str, reason: &str) -> ServiceState {
    let mut detail = Map::new();
    detail.insert("error".into(), json!(reason));
    ServiceState { kind, name: name.into(), running: false, detail }
}

fn compose(node: &Node, name: &str) -> ServiceState {
    let file = match compose_file(node, name) {
        Ok(f) => f,
        Err(e) => return missing("compose", name, &e.message),
    };
    if proc::which("docker").is_none() {
        return missing("compose", name, "docker is not installed");
    }
    let minute = Duration::from_secs(60);
    let declared: Vec<String> =
        match node.exec(&["docker", "compose", "-p", name, "-f", &file, "config", "--services"], minute) {
            Ok(r) if r.exit_code == 0 => {
                r.out().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
            }
            _ => return missing("compose", name, "docker compose config failed"),
        };
    let ps = match node.exec(&["docker", "compose", "-p", name, "-f", &file, "ps", "--all", "--format", "json"], minute)
    {
        Ok(r) if r.exit_code == 0 => r.out(),
        Ok(r) => {
            return missing(
                "compose",
                name,
                &format!("docker compose ps: {}", r.err().trim().lines().last().unwrap_or("")),
            );
        }
        Err(e) => return missing("compose", name, &e.message),
    };
    let containers = compose_containers(&ps);
    let (running, detail) = stack_state(&declared, &containers);
    ServiceState { kind: "compose", name: name.into(), running, detail }
}

/// `docker compose ps --format json`: one JSON array from older Compose, one object per line from newer ones.
fn compose_containers(out: &str) -> Vec<Map<String, Value>> {
    let out = out.trim();
    if out.starts_with('[') {
        serde_json::from_str(out).unwrap_or_default()
    } else {
        out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}

/// Whether a stack runs: every declared service has a container, and each runs and isn't unhealthy, or exited with 0
/// (a one-shot job that finished is not a stack that is down).
fn stack_state(declared: &[String], containers: &[Map<String, Value>]) -> (bool, Map<String, Value>) {
    let field = |c: &Map<String, Value>, k: &str| -> Option<String> {
        c.get(k).and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string())))
    };
    let missing: Vec<&String> =
        declared.iter().filter(|s| !containers.iter().any(|c| field(c, "Service").as_ref() == Some(*s))).collect();
    let finished = |c: &Map<String, Value>| {
        field(c, "State").as_deref() == Some("exited") && field(c, "ExitCode").as_deref() == Some("0")
    };
    let down = containers.iter().any(|c| {
        (field(c, "State").as_deref() != Some("running") && !finished(c))
            || field(c, "Health").as_deref() == Some("unhealthy")
    });
    let rows: Vec<Value> = containers
        .iter()
        .map(|c| {
            json!({
                "service": field(c, "Service"),
                "state": field(c, "State"),
                "health": field(c, "Health").filter(|h| !h.is_empty()),
                "image": field(c, "Image"),
            })
        })
        .collect();
    let mut detail = Map::new();
    detail.insert("containers".into(), Value::Array(rows));
    if !missing.is_empty() {
        detail.insert("missing".into(), json!(missing));
    }
    (missing.is_empty() && !down, detail)
}

fn unit(node: &Node, name: &str) -> ServiceState {
    if system::init() != Init::Systemd {
        return missing("unit", name, "no systemd on this node");
    }
    let props = node
        .exec_ok(&["systemctl", "show", "--no-pager", "--property=LoadState,ActiveState,SubState", "--", name])
        .map(|o| parsers::key_values(&o))
        .unwrap_or_default();
    let mut detail = Map::new();
    for (key, prop) in [("load", "LoadState"), ("active", "ActiveState"), ("sub", "SubState")] {
        detail.insert(key.into(), json!(props.get(prop)));
    }
    ServiceState {
        kind: "unit",
        name: name.into(),
        running: props.get("ActiveState").map(String::as_str) == Some("active"),
        detail,
    }
}

fn procd(node: &Node, name: &str) -> ServiceState {
    if system::init() != Init::Procd {
        return missing("procd", name, "no procd on this node");
    }
    let s = system::procd_services(node, Some(name)).ok().and_then(|mut all| all.remove(name));
    let mut detail = Map::new();
    let active = match &s {
        None => "missing",
        Some(s) if s.state == ProcdState::Failed => "failed",
        Some(_) => "active",
    };
    detail.insert("active".into(), json!(active));
    let instances: Map<String, Value> = s
        .as_ref()
        .map(|s| {
            s.instances.iter().map(|(k, v)| (k.clone(), v.get("running").cloned().unwrap_or(Value::Null))).collect()
        })
        .unwrap_or_default();
    detail.insert("instances".into(), Value::Object(instances));
    ServiceState { kind: "procd", name: name.into(), running: s.is_some_and(|s| s.state != ProcdState::Failed), detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_runs_when_every_service_does_or_finished_well() {
        let declared = vec!["web".to_string(), "migrate".to_string()];
        let ps = r#"{"Service":"web","State":"running","Health":"healthy","Image":"nginx"}
{"Service":"migrate","State":"exited","ExitCode":0,"Image":"app"}"#;
        assert!(stack_state(&declared, &compose_containers(ps)).0);
        let failed = r#"[{"Service":"web","State":"running"},{"Service":"migrate","State":"exited","ExitCode":1}]"#;
        assert!(!stack_state(&declared, &compose_containers(failed)).0);
        let (running, detail) = stack_state(&declared, &compose_containers(r#"{"Service":"web","State":"running"}"#));
        assert!(!running);
        assert_eq!(detail["missing"], json!(["migrate"]));
        let sick = r#"{"Service":"web","State":"running","Health":"unhealthy"}"#;
        assert!(!stack_state(&["web".into()], &compose_containers(sick)).0);
    }
}
