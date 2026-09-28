//! Finds, validates and runs the operator's scripts (spec §6).

use super::{Node, internal};
use crate::os::{fs, proc, sys};
use limen_core::params;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::scripts::{self, Catalog, ScriptKind, ScriptSpec};
use limen_core::trust;
use serde_json::{Map, Value};
use std::time::Duration;

const MAX_HEADER: usize = 64 * 1024;
/// What a check or an action may print before it is stopped; a stream sends it on as it comes instead.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// A file in a script directory: its spec when it is usable, or what is wrong with it. A file whose name is not a
/// script's ([ignored]: a README, a mistyped name) is only reported by `limen lint`.
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    pub file: String,
    pub path: String,
    pub spec: Option<ScriptSpec>,
    pub problem: Option<String>,
    pub ignored: bool,
}

impl ScriptEntry {
    fn usable(file: &str, resolved: String, spec: ScriptSpec) -> Self {
        ScriptEntry { file: file.into(), path: resolved, spec: Some(spec), problem: None, ignored: false }
    }

    fn broken(file: &str, path: &str, problem: String) -> Self {
        ScriptEntry { file: file.into(), path: path.into(), spec: None, problem: Some(problem), ignored: false }
    }

    fn not_a_script(file: &str, path: &str, problem: String) -> Self {
        ScriptEntry { ignored: true, ..ScriptEntry::broken(file, path, problem) }
    }
}

/// Every file in the directory of [kind] but hidden ones (a `.gitkeep`), each with its spec or its problem.
pub fn discover(node: &Node, kind: ScriptKind) -> Vec<ScriptEntry> {
    let dir = node.config.directory(kind);
    let Some(info) = fs::stat(&dir) else { return vec![] };
    if info.kind != fs::FileType::Directory {
        return vec![ScriptEntry::broken(&dir, &dir, format!("{dir} is not a directory"))];
    }
    fs::list(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|file| !file.starts_with('.'))
        .map(|file| read_entry(node, kind, &dir, &file))
        .collect()
}

fn read_entry(node: &Node, kind: ScriptKind, dir: &str, file: &str) -> ScriptEntry {
    let path = format!("{dir}/{file}");
    let Some(name) = scripts::name_of(file, kind) else {
        let shape = if kind == ScriptKind::Setup { "NN-name" } else { "a-z, 0-9, - and _" };
        return ScriptEntry::not_a_script(file, &path, format!("{path}: not a script name ({shape}); ignored"));
    };
    match trusted_spec(node, kind, &name, &path) {
        Ok((resolved, spec)) => ScriptEntry::usable(file, resolved, spec),
        Err(problem) => ScriptEntry::broken(file, &path, format!("{path}: {problem}")),
    }
}

/// The script at [path], resolved, trusted and with its header parsed; what stops it otherwise.
fn trusted_spec(
    node: &Node,
    kind: ScriptKind,
    name: &str,
    path: &str,
) -> std::result::Result<(String, ScriptSpec), String> {
    let resolved = fs::real_path(path).ok_or("cannot resolve")?;
    if let Some(problem) = trust::problem(&fs::chain(&resolved), node.trusted_owner) {
        return Err(problem);
    }
    let header = fs::read(&resolved, MAX_HEADER).ok_or("cannot read")?;
    let spec = scripts::parse(name, kind, &String::from_utf8_lossy(&header))?;
    Ok((resolved, spec))
}

pub fn catalog(node: &Node) -> Catalog {
    let discovered: Vec<(ScriptKind, Vec<ScriptEntry>)> =
        ScriptKind::ALL.into_iter().map(|kind| (kind, discover(node, kind))).collect();
    let specs = |kind: ScriptKind| -> Vec<ScriptSpec> {
        discovered
            .iter()
            .filter(|(entries_kind, _)| *entries_kind == kind)
            .flat_map(|(_, entries)| entries.iter().filter_map(|entry| entry.spec.clone()))
            .collect()
    };
    let problems = discovered
        .iter()
        .flat_map(|(_, entries)| entries)
        .filter(|entry| !entry.ignored)
        .filter_map(|entry| entry.problem.clone())
        .collect();
    Catalog {
        checks: specs(ScriptKind::Check),
        actions: specs(ScriptKind::Action),
        setup: specs(ScriptKind::Setup),
        problems,
    }
}

/// The usable script [name] of [kind]; `not_found` or `unavailable` with the reason otherwise.
pub fn find(node: &Node, kind: ScriptKind, name: &str) -> Result<(ScriptEntry, ScriptSpec)> {
    let named: Vec<ScriptEntry> = discover(node, kind)
        .into_iter()
        .filter(|entry| scripts::name_of(&entry.file, kind).as_deref() == Some(name))
        .collect();
    if named.len() > 1 {
        let files: Vec<&str> = named.iter().map(|entry| entry.file.as_str()).collect();
        return Err(error(ErrorCode::Unavailable, same_name_problem(name, &files)));
    }
    let Some(entry) = named.into_iter().next() else {
        return Err(error(
            ErrorCode::NotFound,
            format!("no {} named '{name}' in {}", kind.name(), node.config.directory(kind)),
        ));
    };
    let spec = entry.spec.clone().ok_or_else(|| {
        error(ErrorCode::Unavailable, entry.problem.clone().unwrap_or_else(|| "unusable script".into()))
    })?;
    Ok((entry, spec))
}

/// Two [files] that are both the script [name]: which one runs would depend on the order of the directory.
pub fn same_name_problem(name: &str, files: &[&str]) -> String {
    format!("{} are both '{name}'; keep one", files.join(" and "))
}

/// The environment of a script: a clean one plus `LIMEN_*` (spec §6). Arguments are validated first.
pub fn environment(spec: &ScriptSpec, args: &Map<String, Value>) -> Result<Vec<String>> {
    let values = params::validate(&spec.params, args)?;
    let mut env = proc::root_env();
    env.push(format!("LIMEN_KIND={}", spec.kind.name()));
    env.push(format!("LIMEN_SCRIPT={}", spec.name));
    env.push(format!("LIMEN_NODE={}", sys::hostname()));
    for (param, value) in values {
        env.push(format!("{}={}", scripts::env_name(&param), env_value(&value)));
    }
    Ok(env)
}

/// An argument as its variable holds it: a string as itself, anything else as JSON.
fn env_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn run(
    entry: &ScriptEntry,
    spec: &ScriptSpec,
    env: Vec<String>,
    max_output: usize,
    on_chunk: Option<proc::OnChunk>,
) -> Result<proc::ProcResult> {
    proc::run(
        std::slice::from_ref(&entry.path),
        proc::Run {
            env,
            timeout: Duration::from_secs(spec.timeout_seconds),
            max_output,
            on_chunk,
            ..Default::default()
        },
    )
    .map_err(internal)
}
