//! The requests the gate answers (spec §4, §5), with their arguments.

use crate::params::{Param, ParamType::*};
use serde_json::json;
use std::sync::LazyLock;

/// A request. [tool] says whether the hub exposes it as an MCP tool as it is; `hello` and `run` are requests the hub
/// turns into `nodes` and one tool per script instead.
#[derive(Debug, Clone)]
pub struct RequestDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<Param>,
    pub tool: bool,
}

const ABSOLUTE_PATH: &str = r"^/[^\x00-\x1f]{0,4095}$";
const GREP_TEXT: &str = r"^[^\x00-\x1f]{1,256}$";
pub const SCRIPT_NAME: &str = "^[a-z0-9][a-z0-9_-]{0,47}$";

/// The arguments limen adds to every script's tool and applies itself, after redaction (spec §5): a script can't
/// declare them, and neither can it declare the hub's `node`.
pub const RESERVED_ARGS: [&str; 3] = ["node", "grep", "tail"];

pub fn all() -> &'static [RequestDef] {
    static ALL: LazyLock<Vec<RequestDef>> = LazyLock::new(|| {
        vec![
            hidden("hello", "Node facts, limen version and the catalog of scripts.", vec![]),
            tool(
                "read_file",
                "Contents of a file the node allows: a range of lines, or its last lines.",
                with_filters(vec![
                    Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH),
                    Param::new("from", Int, "First line, from 1").range(Some(1), None),
                    Param::new("lines", Int, "How many lines from `from`").range(Some(1), Some(20000)),
                ]),
            ),
            tool(
                "list_dir",
                "Entries of a directory the node allows, or that leads to allowed files: name, type, size, owner, \
                 mode, modification time.",
                vec![Param::new("path", String, "Absolute path").required().pattern(ABSOLUTE_PATH)],
            ),
            tool(
                "history",
                "The node's audit log: every request limen answered, with its script, arguments, client and result.",
                vec![
                    Param::new("lines", Int, "How many entries, newest last")
                        .default(json!(50))
                        .range(Some(1), Some(1000)),
                ],
            ),
            hidden(
                "run",
                "Runs one script of the node's packs.",
                with_filters(vec![
                    Param::new("script", String, "The script's name").required().pattern(SCRIPT_NAME),
                    Param::new("args", Object, "The script's arguments"),
                ]),
            ),
        ]
    });
    &ALL
}

/// `grep` and `tail`, which limen applies to what a file holds or a script prints, once redacted.
pub fn filters() -> Vec<Param> {
    vec![
        Param::new("grep", String, "Only lines containing this text, case-insensitive").pattern(GREP_TEXT),
        // No maximum here: the node cuts at its limits.max_lines and says so, whatever the operator set.
        Param::new("tail", Int, "Only the last lines, this many").range(Some(1), None),
    ]
}

fn with_filters(mut params: Vec<Param>) -> Vec<Param> {
    params.extend(filters());
    params
}

fn tool(name: &'static str, description: &'static str, params: Vec<Param>) -> RequestDef {
    RequestDef { name, description, params, tool: true }
}

fn hidden(name: &'static str, description: &'static str, params: Vec<Param>) -> RequestDef {
    RequestDef { name, description, params, tool: false }
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
    fn every_pattern_compiles() {
        for request in all() {
            for pattern in request.params.iter().filter_map(|param| param.pattern.as_deref()) {
                crate::params::full_match(pattern)
                    .unwrap_or_else(|error| panic!("{} {pattern}: {error}", request.name));
            }
        }
    }

    #[test]
    fn the_filters_are_reserved() {
        assert!(filters().iter().all(|param| RESERVED_ARGS.contains(&param.name.as_str())));
    }
}
