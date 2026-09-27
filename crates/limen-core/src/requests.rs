//! The requests the gate answers (spec §4, §5), with their arguments.

use crate::params::{Param, ParamType::*};
use serde_json::json;
use std::sync::OnceLock;

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
const TIME: &str = r"^(\d{1,6}[smhd]|\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?Z)$";
pub const SCRIPT_NAME: &str = "^[a-z0-9][a-z0-9_-]{0,47}$";

fn def(name: &'static str, role: Role, description: &'static str, params: Vec<Param>) -> RequestDef {
    RequestDef { name, role, description, params, tool: true }
}

fn hidden(mut d: RequestDef) -> RequestDef {
    d.tool = false;
    d
}

pub fn all() -> &'static [RequestDef] {
    static ALL: OnceLock<Vec<RequestDef>> = OnceLock::new();
    ALL.get_or_init(|| {
        let time = "Relative (`30m`, `2h`, `1d`: that long ago) or absolute UTC (`2026-09-26T08:00:00Z`)";
        vec![
            hidden(def("hello", Role::Read, "Node facts, limen version and the catalog of scripts.", vec![])),
            def(
                "status",
                Role::Read,
                "Overview of a node: uptime, load, memory, disk usage per mount, failed services (systemd, or procd on \
                 OpenWrt), containers that are not running or unhealthy, and whether a reboot is pending. Start here.",
                vec![],
            ),
            def(
                "services",
                Role::Read,
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
            def(
                "service",
                Role::Read,
                "One service with its last log lines: a systemd unit's state, result, restarts, main PID, memory, unit \
                 file and enablement, or a procd service's instances on OpenWrt.",
                vec![
                    Param::new("name", String, "Unit or service name; `.service` is assumed without a suffix")
                        .required()
                        .pattern(UNIT),
                    Param::new("lines", Int, "Journal lines to include").default(json!(20)).range(Some(0), Some(200)),
                ],
            ),
            def(
                "containers",
                Role::Read,
                "Docker containers: name, image, state, health, restarts, start time and compose project.",
                vec![Param::new("all", Bool, "Include stopped containers").default(json!(true))],
            ),
            def(
                "container",
                Role::Read,
                "One Docker container: image and digest, state, health checks, mounts, ports, networks, labels and \
                 restart policy. Environment variables are listed by name only.",
                vec![Param::new("name", String, "Container name or ID").required().pattern(CONTAINER)],
            ),
            def(
                "logs",
                Role::Read,
                "Log lines from a service (systemd's journal, or logread on OpenWrt), the whole log, a Docker container, \
                 or an allowed file. Always a bounded window: the last `lines` lines, optionally within `since`/`until` \
                 and matching `grep`.",
                vec![
                    Param::new("source", Enum, "Where to read").required().values(&["unit", "journal", "container", "file"]),
                    Param::new("name", String, "Unit name, container name, or absolute file path. Not used with `journal`")
                        .pattern(r"^[^\x00-\x1f]{1,4096}$"),
                    Param::new("since", String, &format!("Start of the window. {time}. Not for files")).pattern(TIME),
                    Param::new("until", String, &format!("End of the window. {time}. Not for files")).pattern(TIME),
                    // No maximum here: the node cuts at its logs.max_lines and says so, whatever the operator set.
                    Param::new("lines", Int, "How many lines, newest last").default(json!(200)).range(Some(1), None),
                    Param::new("grep", String, "Only lines containing this text, case-insensitive")
                        .pattern(r"^[^\x00-\x1f]{1,256}$"),
                    Param::new("priority", Enum, "Journal only: this priority and more severe")
                        .values(&["emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"]),
                ],
            ),
            def(
                "read_file",
                Role::Read,
                "Contents of a file the node allows, by line range.",
                vec![
                    Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH),
                    Param::new("from", Int, "First line, from 1").default(json!(1)).range(Some(1), None),
                    Param::new("lines", Int, "How many lines").default(json!(500)).range(Some(1), Some(20000)),
                ],
            ),
            def(
                "list_dir",
                Role::Read,
                "Entries of a directory the node allows, or that leads to allowed files: name, type, size, owner, mode, \
                 modification time.",
                vec![Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH)],
            ),
            def(
                "processes",
                Role::Read,
                "Top processes by CPU or memory.",
                vec![
                    Param::new("sort", Enum, "Order").default(json!("cpu")).values(&["cpu", "memory"]),
                    Param::new("limit", Int, "How many").default(json!(20)).range(Some(1), Some(200)),
                ],
            ),
            def("ports", Role::Read, "Listening TCP and UDP sockets and the processes behind them.", vec![]),
            def(
                "history",
                Role::Read,
                "The node's audit log: every request limen answered, with role, arguments, client and result.",
                vec![Param::new("lines", Int, "How many entries, newest last").default(json!(50)).range(Some(1), Some(1000))],
            ),
            def(
                "state",
                Role::Read,
                "What is deployed against what should be: the repository commit on the node and on the remote, and \
                 every service node.toml expects (compose stacks, systemd units, procd services) with whether it runs.",
                vec![],
            ),
            hidden(def(
                "check",
                Role::Read,
                "Runs a check script.",
                vec![
                    Param::new("name", String, "Check name").required().pattern(SCRIPT_NAME),
                    Param::new("args", Object, "The check's arguments"),
                ],
            )),
            hidden(def("sync", Role::Deploy, "Brings the node's copy of the repository to the remote branch.", vec![])),
            hidden(def(
                "apply",
                Role::Deploy,
                "Runs every setup script in order.",
                vec![
                    Param::new("from", String, "Start at the script with this prefix").pattern("^[0-9]{1,4}$"),
                    Param::new("dry_run", Bool, "List what would run").default(json!(false)),
                    Param::new("sync", Bool, "Sync the repository first").default(json!(true)),
                ],
            )),
            hidden(def(
                "action",
                Role::Deploy,
                "Runs one action script.",
                vec![
                    Param::new("name", String, "Action name").required().pattern(SCRIPT_NAME),
                    Param::new("args", Object, "The action's arguments"),
                ],
            )),
        ]
    })
}

pub fn find(name: &str) -> Option<&'static RequestDef> {
    all().iter().find(|d| d.name == name)
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
        let deploy: Vec<_> = all().iter().filter(|d| d.role == Role::Deploy).collect();
        assert_eq!(deploy.iter().map(|d| d.name).collect::<Vec<_>>(), ["sync", "apply", "action"]);
        assert!(deploy.iter().all(|d| !d.tool));
    }

    #[test]
    fn every_pattern_compiles() {
        for d in all() {
            for p in d.params.iter().filter_map(|p| p.pattern.as_deref()) {
                crate::params::full_match(p).unwrap_or_else(|e| panic!("{} {p}: {e}", d.name));
            }
        }
    }
}
