//! The requests the gate answers (spec §4, §5), with their arguments.

use crate::params::{Param, ParamType::*};
use crate::system::parsers::PRIORITIES;
use serde_json::json;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Read,
    Deploy,
}

impl Role {
    pub fn wire(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Deploy => "deploy",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        match text {
            "read" => Some(Role::Read),
            "deploy" => Some(Role::Deploy),
            _ => None,
        }
    }
}

/// A request. [tool] says whether the hub exposes it as an MCP tool as it is; `hello` and `check` are requests the
/// hub turns into `nodes` and `check_<name>` instead.
#[derive(Debug, Clone)]
pub struct RequestDef {
    pub name: &'static str,
    pub role: Role,
    pub description: &'static str,
    pub params: Vec<Param>,
    pub tool: bool,
}

pub const UNIT: &str = r"^[A-Za-z0-9@._:\\-]{1,256}$";
const UNIT_GLOB: &str = r"^[A-Za-z0-9@._:*?\[\]\\-]{1,256}$";
pub const CONTAINER: &str = "^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$";
const ABSOLUTE_PATH: &str = r"^/[^\x00-\x1f]{0,4095}$";
const LOG_SOURCE_NAME: &str = r"^[^\x00-\x1f]{1,4096}$";
const GREP_TEXT: &str = r"^[^\x00-\x1f]{1,256}$";
const TIME: &str = r"^(\d{1,6}[smhd]|\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?Z)$";
const TIME_FORMS: &str = "Relative (`30m`, `2h`, `1d`: that long ago) or absolute UTC (`2026-09-26T08:00:00Z`)";
pub const SCRIPT_NAME: &str = "^[a-z0-9][a-z0-9_-]{0,47}$";

pub fn all() -> &'static [RequestDef] {
    static ALL: LazyLock<Vec<RequestDef>> = LazyLock::new(|| {
        [
            node_requests(),
            service_requests(),
            container_requests(),
            log_requests(),
            file_requests(),
            process_requests(),
            record_requests(),
            script_requests(),
        ]
        .concat()
    });
    &ALL
}

/// A read request the hub exposes as an MCP tool as it is. Only the read role's requests are ever tools.
fn tool(name: &'static str, description: &'static str, params: Vec<Param>) -> RequestDef {
    RequestDef { name, role: Role::Read, description, params, tool: true }
}

/// A request the hub doesn't expose as it is: see [RequestDef].
fn hidden(name: &'static str, role: Role, description: &'static str, params: Vec<Param>) -> RequestDef {
    RequestDef { name, role, description, params, tool: false }
}

fn node_requests() -> Vec<RequestDef> {
    vec![
        hidden("hello", Role::Read, "Node facts, limen version and the catalog of scripts.", vec![]),
        tool(
            "status",
            "Overview of a node: uptime, load, memory, disk usage per mount, failed services (systemd, or procd on \
             OpenWrt), containers that are not running or unhealthy, and whether a reboot is pending. Start here.",
            vec![],
        ),
    ]
}

fn service_requests() -> Vec<RequestDef> {
    vec![
        tool(
            "services",
            "Services with their state: systemd units (load, active and sub state), or procd services on OpenWrt. \
             `type` is systemd's.",
            vec![
                Param::new("state", Enum, "Filter by state")
                    .default(json!("all"))
                    .values(&["all", "running", "failed", "active", "inactive", "exited"]),
                Param::new("type", Enum, "Unit type")
                    .default(json!("service"))
                    .values(&["service", "timer", "socket", "mount", "path", "target", "all"]),
                Param::new("pattern", String, "Glob on the unit name, e.g. `docker*`").pattern(UNIT_GLOB),
            ],
        ),
        tool(
            "service",
            "One service with its last log lines: a systemd unit's state, result, restarts, main PID, memory, unit \
             file and enablement, or a procd service's instances on OpenWrt.",
            vec![
                Param::new("name", String, "Unit or service name; `.service` is assumed without a suffix")
                    .required()
                    .pattern(UNIT),
                Param::new("lines", Int, "Journal lines to include").default(json!(20)).range(Some(0), Some(200)),
            ],
        ),
    ]
}

fn container_requests() -> Vec<RequestDef> {
    vec![
        tool(
            "containers",
            "Docker containers: name, image, state, health, restarts, start time and compose project.",
            vec![Param::new("all", Bool, "Include stopped containers").default(json!(true))],
        ),
        tool(
            "container",
            "One Docker container: image and digest, state, health checks, mounts, ports, networks, labels and \
             restart policy. Environment variables are listed by name only.",
            vec![Param::new("name", String, "Container name or ID").required().pattern(CONTAINER)],
        ),
    ]
}

fn log_requests() -> Vec<RequestDef> {
    vec![tool(
        "logs",
        "Log lines from a service (systemd's journal, or logread on OpenWrt), the whole log, a Docker container, or \
         an allowed file. Always a bounded window: the last `lines` lines, optionally within `since`/`until` and \
         matching `grep`.",
        vec![
            Param::new("source", Enum, "Where to read").required().values(&["unit", "journal", "container", "file"]),
            Param::new("name", String, "Unit name, container name, or absolute file path. Not used with `journal`")
                .pattern(LOG_SOURCE_NAME),
            Param::new("since", String, &format!("Start of the window. {TIME_FORMS}. Not for files")).pattern(TIME),
            Param::new("until", String, &format!("End of the window. {TIME_FORMS}. Not for files")).pattern(TIME),
            // No maximum here: the node cuts at its logs.max_lines and says so, whatever the operator set.
            Param::new("lines", Int, "How many lines, newest last").default(json!(200)).range(Some(1), None),
            Param::new("grep", String, "Only lines containing this text, case-insensitive").pattern(GREP_TEXT),
            Param::new("priority", Enum, "Journal only: this priority and more severe").values(&PRIORITIES),
        ],
    )]
}

fn file_requests() -> Vec<RequestDef> {
    vec![
        tool(
            "read_file",
            "Contents of a file the node allows, by line range.",
            vec![
                Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH),
                Param::new("from", Int, "First line, from 1").default(json!(1)).range(Some(1), None),
                Param::new("lines", Int, "How many lines").default(json!(500)).range(Some(1), Some(20000)),
            ],
        ),
        tool(
            "list_dir",
            "Entries of a directory the node allows, or that leads to allowed files: name, type, size, owner, mode, \
             modification time.",
            vec![Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH)],
        ),
    ]
}

fn process_requests() -> Vec<RequestDef> {
    vec![
        tool(
            "processes",
            "Top processes by CPU or memory.",
            vec![
                Param::new("sort", Enum, "Order").default(json!("cpu")).values(&["cpu", "memory"]),
                Param::new("limit", Int, "How many").default(json!(20)).range(Some(1), Some(200)),
            ],
        ),
        tool("ports", "Listening TCP and UDP sockets and the processes behind them.", vec![]),
    ]
}

/// What limen itself keeps on the node: its audit log, and what it deployed.
fn record_requests() -> Vec<RequestDef> {
    vec![
        tool(
            "history",
            "The node's audit log: every request limen answered, with role, arguments, client and result.",
            vec![
                Param::new("lines", Int, "How many entries, newest last").default(json!(50)).range(Some(1), Some(1000)),
            ],
        ),
        tool(
            "state",
            "What is deployed against what should be: the repository commit on the node and on the remote, and \
             every service node.toml expects (compose stacks, systemd units, procd services) with whether it runs.",
            vec![],
        ),
    ]
}

/// The operator's scripts and their repository (spec §6): a check becomes a `check_<name>` tool on the hub; the deploy
/// role's requests are never tools.
fn script_requests() -> Vec<RequestDef> {
    vec![
        hidden(
            "check",
            Role::Read,
            "Runs a check script.",
            vec![
                Param::new("name", String, "Check name").required().pattern(SCRIPT_NAME),
                Param::new("args", Object, "The check's arguments"),
            ],
        ),
        hidden("sync", Role::Deploy, "Brings the node's copy of the repository to the remote branch.", vec![]),
        hidden(
            "apply",
            Role::Deploy,
            "Runs every setup script in order.",
            vec![
                Param::new("from", String, "Start at the script with this prefix").pattern("^[0-9]{1,4}$"),
                Param::new("dry_run", Bool, "List what would run").default(json!(false)),
                Param::new("sync", Bool, "Sync the repository first").default(json!(true)),
            ],
        ),
        hidden(
            "action",
            Role::Deploy,
            "Runs one action script.",
            vec![
                Param::new("name", String, "Action name").required().pattern(SCRIPT_NAME),
                Param::new("args", Object, "The action's arguments"),
            ],
        ),
    ]
}

pub fn find(name: &str) -> Option<&'static RequestDef> {
    all().iter().find(|request| request.name == name)
}

/// A request by name that is known to exist: the constants of this module.
pub fn named(name: &str) -> &'static RequestDef {
    find(name).unwrap_or_else(|| panic!("no request {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_requests_are_never_tools() {
        let deploy: Vec<_> = all().iter().filter(|request| request.role == Role::Deploy).collect();
        assert_eq!(deploy.iter().map(|request| request.name).collect::<Vec<_>>(), ["sync", "apply", "action"]);
        assert!(deploy.iter().all(|request| !request.tool));
    }

    #[test]
    fn every_pattern_compiles() {
        for request in all() {
            for pattern in request.params.iter().filter_map(|param| param.pattern.as_deref()) {
                crate::params::full_match(pattern)
                    .unwrap_or_else(|error| panic!("{} {pattern}: {error}", request.name));
            }
        }
    }
}
