//! Finds, validates and runs the operator's scripts (spec §6).

use super::Node;
use crate::os::{fs, proc, sys};
use limen_core::params;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::scripts::{self, Catalog, ScriptKind, ScriptSpec};
use limen_core::trust;
use serde_json::{Map, Value};
use std::time::Duration;

const MAX_HEADER: usize = 64 * 1024;

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

pub fn discover(node: &Node, kind: ScriptKind) -> Vec<ScriptEntry> {
    let dir = node.config.directory(kind);
    let Some(info) = fs::stat(&dir) else { return vec![] };
    if info.kind != fs::FileType::Directory {
        return vec![entry_problem(&dir, &dir, format!("{dir} is not a directory"), false)];
    }
    fs::list(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !f.starts_with('.'))
        .map(|file| entry(node, kind, &dir, &file))
        .collect()
}

fn entry_problem(file: &str, path: &str, problem: String, ignored: bool) -> ScriptEntry {
    ScriptEntry { file: file.into(), path: path.into(), spec: None, problem: Some(problem), ignored }
}

fn entry(node: &Node, kind: ScriptKind, dir: &str, file: &str) -> ScriptEntry {
    let path = format!("{dir}/{file}");
    let Some(name) = scripts::name_of(file, kind) else {
        let shape = if kind == ScriptKind::Setup { "NN-name" } else { "a-z, 0-9, - and _" };
        return entry_problem(file, &path, format!("{path}: not a script name ({shape}); ignored"), true);
    };
    let Some(resolved) = fs::real_path(&path) else {
        return entry_problem(file, &path, format!("{path}: cannot resolve"), false);
    };
    if let Some(p) = trust::problem(&fs::chain(&resolved), node.trusted_owner) {
        return entry_problem(file, &path, format!("{path}: {p}"), false);
    }
    let Some(bytes) = fs::read(&resolved, MAX_HEADER) else {
        return entry_problem(file, &path, format!("{path}: cannot read"), false);
    };
    match scripts::parse(&name, kind, &String::from_utf8_lossy(&bytes)) {
        Ok(spec) => ScriptEntry { file: file.into(), path: resolved, spec: Some(spec), problem: None, ignored: false },
        Err(e) => entry_problem(file, &path, format!("{path}: {e}"), false),
    }
}

pub fn catalog(node: &Node) -> Catalog {
    let all: Vec<(ScriptKind, Vec<ScriptEntry>)> = ScriptKind::ALL.iter().map(|k| (*k, discover(node, *k))).collect();
    let specs = |kind: ScriptKind| -> Vec<ScriptSpec> {
        all.iter().filter(|(k, _)| *k == kind).flat_map(|(_, e)| e.iter().filter_map(|e| e.spec.clone())).collect()
    };
    Catalog {
        checks: specs(ScriptKind::Check),
        actions: specs(ScriptKind::Action),
        setup: specs(ScriptKind::Setup),
        problems: all
            .iter()
            .flat_map(|(_, e)| e.iter().filter(|e| !e.ignored).filter_map(|e| e.problem.clone()))
            .collect(),
    }
}

/// The usable script [name] of [kind]; `not_found` or `unavailable` with the reason otherwise.
pub fn find(node: &Node, kind: ScriptKind, name: &str) -> Result<(ScriptEntry, ScriptSpec)> {
    let same: Vec<ScriptEntry> =
        discover(node, kind).into_iter().filter(|e| scripts::name_of(&e.file, kind).as_deref() == Some(name)).collect();
    let Some(entry) = same.first().cloned() else {
        return Err(error(
            ErrorCode::NotFound,
            format!("no {} named '{name}' in {}", kind.name(), node.config.directory(kind)),
        ));
    };
    if same.len() > 1 {
        let files: Vec<&str> = same.iter().map(|e| e.file.as_str()).collect();
        return Err(error(ErrorCode::Unavailable, format!("{} are both '{name}'; keep one", files.join(" and "))));
    }
    let spec = entry
        .spec
        .clone()
        .ok_or_else(|| error(ErrorCode::Unavailable, entry.problem.clone().unwrap_or("unusable script".into())))?;
    Ok((entry, spec))
}

/// The environment of a script: a clean one plus `LIMEN_*` (spec §6). Arguments are validated first.
pub fn environment(spec: &ScriptSpec, args: &Map<String, Value>) -> Result<Vec<String>> {
    let values = params::validate(&spec.params, args)?;
    let mut env = proc::root_env();
    env.push(format!("LIMEN_KIND={}", spec.kind.name()));
    env.push(format!("LIMEN_SCRIPT={}", spec.name));
    env.push(format!("LIMEN_NODE={}", sys::hostname()));
    for (k, v) in values {
        let text = match &v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        env.push(format!("{}={text}", scripts::env_name(&k)));
    }
    Ok(env)
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
    .map_err(|e| error(ErrorCode::Internal, e))
}
