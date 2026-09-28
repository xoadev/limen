//! `state` (spec §6.1): the repository commit on the node against the remote, and every service `node.toml` expects
//! against what runs. What `apply` checks at the end, and what an agent asks first after a deploy.

use super::init::{self, Init};
use super::procd::{self, ProcdState};
use super::{Answer, Node, repo};
use crate::os::{fs, proc};
use limen_core::config::expectations::Expectations;
use limen_core::config::node::RepoConfig;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::redactor::Redactor;
use limen_core::system::parsers;
use serde_json::{Map, Value, json};
use std::time::Duration;

const COMPOSE_FILES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"];
const COMPOSE_TIMEOUT: Duration = Duration::from_secs(60);

/// One expected service and what the node says about it.
pub struct ServiceState {
    pub kind: &'static str,
    pub name: String,
    pub running: bool,
    pub detail: Map<String, Value>,
}

impl ServiceState {
    /// A service the node couldn't ask about, or that isn't there: not running, and why.
    fn error(kind: &'static str, name: &str, message: &str) -> Self {
        let mut detail = Map::new();
        detail.insert("error".into(), json!(message));
        ServiceState { kind, name: name.into(), running: false, detail }
    }

    fn into_json(self) -> Value {
        let mut object = Map::new();
        object.insert("kind".into(), json!(self.kind));
        object.insert("name".into(), json!(self.name));
        object.insert("running".into(), json!(self.running));
        object.extend(self.detail);
        Value::Object(object)
    }
}

pub fn answer(node: &Node) -> Answer {
    let repo_state = node.config.repo.as_ref().map(RepoState::of);
    let mut problems: Vec<String> = repo_state.as_ref().and_then(RepoState::problem).into_iter().collect();
    let services = services(node).unwrap_or_else(|failure| {
        problems.push(failure.message);
        vec![]
    });
    problems.extend(
        services
            .iter()
            .filter(|service| !service.running)
            .map(|service| format!("{} {} is not running", service.kind, service.name)),
    );
    Answer::of(json!({
        "repo": repo_state.map_or(Value::Null, |state| state.to_json(&node.redactor)),
        "services": services.into_iter().map(ServiceState::into_json).collect::<Vec<_>>(),
        "problems": problems,
    }))
}

pub fn expectations(node: &Node) -> Result<Expectations> {
    let Some(path) = node.config.expectations() else { return Ok(Expectations::default()) };
    if !fs::exists(&path) {
        return Ok(Expectations::default());
    }
    let text = node
        .trusted_text(&path)
        .map_err(|failure| error(ErrorCode::Internal, format!("{path}: {}", failure.message)))?;
    Expectations::parse(&text).map_err(|problem| error(ErrorCode::Internal, format!("{path}: {problem}")))
}

pub fn services(node: &Node) -> Result<Vec<ServiceState>> {
    let expected = expectations(node)?;
    let stacks = expected.compose.iter().map(|name| compose_service(node, name));
    let units = expected.units.iter().map(|name| unit_service(node, name));
    let procd = expected.procd.iter().map(|name| procd_service(node, name));
    Ok(stacks.chain(units).chain(procd).collect())
}

/// `stacks/<name>/compose.yaml` of the node's folder: the file limen brings up and then asks about.
pub fn compose_file(node: &Node, name: &str) -> Result<String> {
    let stacks =
        node.config.stacks().ok_or_else(|| error(ErrorCode::Unavailable, "stacks need [repo] in limen.toml"))?;
    let file = COMPOSE_FILES
        .iter()
        .map(|file_name| format!("{stacks}/{name}/{file_name}"))
        .find(|path| fs::exists(path))
        .ok_or_else(|| error(ErrorCode::NotFound, format!("no compose file in {stacks}/{name}")))?;
    // Read only to refuse a file someone other than root could have written: Compose runs what it says as root.
    node.trusted_text(&file).map_err(|failure| error(ErrorCode::Internal, format!("{file}: {}", failure.message)))?;
    Ok(file)
}

/// The checkout and the remote branch, asked once: reported as `repo`, and among the problems when they differ.
struct RepoState<'a> {
    config: &'a RepoConfig,
    deployed: Option<repo::Commit>,
    remote: Result<String>,
    last_sync: Option<Value>,
}

impl<'a> RepoState<'a> {
    fn of(config: &'a RepoConfig) -> Self {
        RepoState {
            config,
            deployed: repo::deployed(config),
            remote: repo::remote(config),
            last_sync: repo::last_sync(config),
        }
    }

    fn problem(&self) -> Option<String> {
        match (&self.deployed, &self.remote) {
            (None, _) => Some("the repository is not checked out: run sync or apply".into()),
            (Some(commit), Ok(remote)) if &commit.hash != remote => {
                Some(format!("the node is behind {}", self.config.branch))
            }
            _ => None,
        }
    }

    fn to_json(&self, redactor: &Redactor) -> Value {
        let deployed = self
            .deployed
            .as_ref()
            .map(|commit| json!({"commit": commit.hash, "date": commit.date, "subject": commit.subject}));
        let mut state = json!({
            "url": self.config.display_url(),
            "branch": self.config.branch,
            "path": self.config.path,
            "deployed": deployed,
        });
        match &self.remote {
            Ok(remote) => {
                state["remote"] = json!(remote);
                state["up_to_date"] = json!(self.deployed.as_ref().map(|commit| &commit.hash) == Some(remote));
            }
            Err(failure) => state["remote_error"] = json!(redactor.redact(&failure.message)),
        }
        // git's own words, which can hold a URL with its credentials.
        state["last_sync"] = self
            .last_sync
            .as_ref()
            .and_then(|record| serde_json::from_str(&redactor.redact(&record.to_string())).ok())
            .unwrap_or(Value::Null);
        state
    }
}

fn compose_service(node: &Node, name: &str) -> ServiceState {
    match ask_compose(node, name) {
        Ok((running, detail)) => ServiceState { kind: "compose", name: name.into(), running, detail },
        Err(message) => ServiceState::error("compose", name, &message),
    }
}

/// Whether a stack runs and what its containers say, or why Compose couldn't tell.
fn ask_compose(node: &Node, name: &str) -> std::result::Result<(bool, Map<String, Value>), String> {
    let file = compose_file(node, name).map_err(|failure| failure.message)?;
    if proc::which("docker").is_none() {
        return Err("docker is not installed".into());
    }
    let declared = match compose(node, name, &file, &["config", "--services"]) {
        Ok(result) if result.exit_code == 0 => {
            result.out().lines().map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect::<Vec<_>>()
        }
        _ => return Err("docker compose config failed".into()),
    };
    let listed = match compose(node, name, &file, &["ps", "--all", "--format", "json"]) {
        Ok(result) if result.exit_code == 0 => result.out(),
        Ok(result) => return Err(format!("docker compose ps: {}", result.failure_reason())),
        Err(failure) => return Err(failure.message),
    };
    Ok(stack_state(&declared, &compose_containers(&listed)))
}

fn compose(node: &Node, stack: &str, file: &str, command: &[&str]) -> Result<proc::ProcResult> {
    let argv: Vec<&str> = std::iter::once("docker").chain(compose_args(stack, file, command)).collect();
    node.exec(&argv, COMPOSE_TIMEOUT)
}

/// `docker`'s arguments for [command] on one stack: its project named after the stack, its file the repository's.
pub fn compose_args<'a>(stack: &'a str, file: &'a str, command: &[&'a str]) -> Vec<&'a str> {
    ["compose", "-p", stack, "-f", file].into_iter().chain(command.iter().copied()).collect()
}

/// `docker compose ps --format json`: one JSON array from older Compose, one object per line from newer ones.
fn compose_containers(listed: &str) -> Vec<Map<String, Value>> {
    let listed = listed.trim();
    if listed.starts_with('[') {
        serde_json::from_str(listed).unwrap_or_default()
    } else {
        listed.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }
}

/// Whether a stack runs: every declared service has a container, and each runs and isn't unhealthy, or exited with 0
/// (a one-shot job that finished is not a stack that is down).
fn stack_state(declared: &[String], containers: &[Map<String, Value>]) -> (bool, Map<String, Value>) {
    let missing: Vec<&String> = declared
        .iter()
        .filter(|service| !containers.iter().any(|container| field(container, "Service").as_ref() == Some(*service)))
        .collect();
    let any_down = containers.iter().any(is_down);
    let rows: Vec<Value> = containers
        .iter()
        .map(|container| {
            json!({
                "service": field(container, "Service"),
                "state": field(container, "State"),
                "health": field(container, "Health").filter(|health| !health.is_empty()),
                "image": field(container, "Image"),
            })
        })
        .collect();
    let mut detail = Map::new();
    detail.insert("containers".into(), Value::Array(rows));
    if !missing.is_empty() {
        detail.insert("missing".into(), json!(missing));
    }
    (missing.is_empty() && !any_down, detail)
}

fn is_down(container: &Map<String, Value>) -> bool {
    let finished_well =
        field(container, "State").as_deref() == Some("exited") && field(container, "ExitCode").as_deref() == Some("0");
    (field(container, "State").as_deref() != Some("running") && !finished_well)
        || field(container, "Health").as_deref() == Some("unhealthy")
}

/// A field of a container as text: Compose gives most as strings and `ExitCode` as a number.
fn field(container: &Map<String, Value>, key: &str) -> Option<String> {
    container
        .get(key)
        .and_then(|value| value.as_str().map(String::from).or_else(|| value.as_i64().map(|number| number.to_string())))
}

fn unit_service(node: &Node, name: &str) -> ServiceState {
    if init::detect() != Init::Systemd {
        return ServiceState::error("unit", name, "no systemd on this node");
    }
    let properties = node
        .exec_ok(&["systemctl", "show", "--no-pager", "--property=LoadState,ActiveState,SubState", "--", name])
        .map(|output| parsers::key_values(&output))
        .unwrap_or_default();
    let mut detail = Map::new();
    for (key, property) in [("load", "LoadState"), ("active", "ActiveState"), ("sub", "SubState")] {
        detail.insert(key.into(), json!(properties.get(property)));
    }
    ServiceState {
        kind: "unit",
        name: name.into(),
        running: properties.get("ActiveState").map(String::as_str) == Some("active"),
        detail,
    }
}

fn procd_service(node: &Node, name: &str) -> ServiceState {
    if init::detect() != Init::Procd {
        return ServiceState::error("procd", name, "no procd on this node");
    }
    let service = procd::services(node, Some(name)).ok().and_then(|mut services| services.remove(name));
    let failed = service.as_ref().is_some_and(|service| service.state == ProcdState::Failed);
    let active = match &service {
        None => "missing",
        Some(_) if failed => "failed",
        Some(_) => "active",
    };
    let instances: Map<String, Value> = service
        .as_ref()
        .map(|service| {
            service
                .instances
                .iter()
                .map(|(instance, info)| (instance.clone(), info.get("running").cloned().unwrap_or(Value::Null)))
                .collect()
        })
        .unwrap_or_default();
    let mut detail = Map::new();
    detail.insert("active".into(), json!(active));
    detail.insert("instances".into(), Value::Object(instances));
    ServiceState { kind: "procd", name: name.into(), running: service.is_some() && !failed, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_runs_when_every_service_does_or_finished_well() {
        let declared = vec!["web".to_string(), "migrate".to_string()];
        let listed = r#"{"Service":"web","State":"running","Health":"healthy","Image":"nginx"}
{"Service":"migrate","State":"exited","ExitCode":0,"Image":"app"}"#;
        assert!(stack_state(&declared, &compose_containers(listed)).0);
        let failed = r#"[{"Service":"web","State":"running"},{"Service":"migrate","State":"exited","ExitCode":1}]"#;
        assert!(!stack_state(&declared, &compose_containers(failed)).0);
        let (running, detail) = stack_state(&declared, &compose_containers(r#"{"Service":"web","State":"running"}"#));
        assert!(!running);
        assert_eq!(detail["missing"], json!(["migrate"]));
        let sick = r#"{"Service":"web","State":"running","Health":"unhealthy"}"#;
        assert!(!stack_state(&["web".into()], &compose_containers(sick)).0);
    }
}
