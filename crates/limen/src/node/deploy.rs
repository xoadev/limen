//! The deploy role (spec §6): `apply` runs the setup scripts in order, `action` one action. Output goes to stdout as
//! it comes and to `runs/<time>-<name>.log`; the answer is the exit code. Used by the gate and by `limen apply` /
//! `limen action` on the node, so both paths behave the same.

use super::scripts::{self as node_scripts, ScriptEntry};
use super::{Node, repo, state};
use crate::os::{fs, proc, sys};
use limen_core::params::ArgsExt;
use limen_core::protocol::Result;
use limen_core::scripts::{ScriptKind, ScriptSpec};
use limen_core::time::iso;
use serde_json::{Map, Value};
use std::time::Duration;

pub fn run(node: &Node, request: &str, args: &Map<String, Value>) -> Result<bool> {
    Ok(match request {
        "sync" => sync(node),
        "apply" => {
            apply(node, args.string("from"), args.bool("dry_run").unwrap_or(false), args.bool("sync").unwrap_or(true))
        }
        "action" => action(node, args.string("name").unwrap_or_default(), &args.obj("args"))?,
        other => unreachable!("not a deploy request: {other}"),
    })
}

fn say(text: &str) {
    sys::out(text);
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

pub fn sync(node: &Node) -> bool {
    let Some(repo) = &node.config.repo else {
        say("limen: no [repo] in limen.toml; nothing to sync\n");
        return false;
    };
    let path = if repo.path.is_empty() { String::new() } else { format!(" ({})", repo.path) };
    say(&format!("==> sync {} {}{path}\n", repo.display_url(), repo.branch));
    match repo::sync(node, repo) {
        Ok((from, to)) => {
            if from.as_deref() == Some(to.as_str()) {
                say(&format!("<== sync: already at {}\n", short(&to)));
            } else {
                say(&format!("<== sync: {} -> {}\n", from.as_deref().map_or("nothing", short), short(&to)));
            }
            true
        }
        Err(e) => {
            say(&format!("<== sync: FAILED ({})\n", e.message));
            false
        }
    }
}

/// From the repository to a running node (spec §6.1): sync, the setup scripts in order, the compose stacks node.toml
/// declares, and then `state`, which must say that everything expected runs.
pub fn apply(node: &Node, from: Option<&str>, dry_run: bool, sync_first: bool) -> bool {
    if sync_first && !dry_run && node.config.repo.is_some() && !sync(node) {
        return false;
    }
    let entries: Vec<ScriptEntry> =
        node_scripts::discover(node, ScriptKind::Setup).into_iter().filter(|e| !e.ignored).collect();
    // Every script is validated before the first runs: a broken one halfway would leave the machine half done.
    let problems: Vec<&String> = entries.iter().filter_map(|e| e.problem.as_ref()).collect();
    if !problems.is_empty() {
        let list: Vec<String> = problems.iter().map(|p| format!("  {p}")).collect();
        say(&format!("limen: apply stopped before running anything:\n{}\n", list.join("\n")));
        return false;
    }
    let padded = |s: &str| format!("{s:0>4}");
    let selected: Vec<&ScriptEntry> = entries
        .iter()
        .filter(|e| from.is_none_or(|f| padded(e.file.split('-').next().unwrap_or("")) >= padded(f)))
        .collect();
    let stacks = match state::expectations(node) {
        Ok(e) => e.compose,
        Err(e) => {
            say(&format!("limen: apply stopped: {}\n", e.message));
            return false;
        }
    };
    if dry_run {
        for e in &selected {
            let spec = e.spec.as_ref().expect("validated above");
            say(&format!("would run {}: {}\n", spec.name, spec.description));
        }
        for s in &stacks {
            say(&format!("would bring up stack {s}\n"));
        }
        return true;
    }
    for entry in &selected {
        let spec = entry.spec.as_ref().expect("validated above");
        if !run_one(node, entry, spec, &Map::new()) {
            say(&format!(
                "limen: apply stopped at {}; resume with --from {}\n",
                spec.name,
                spec.name.split('-').next().unwrap_or("")
            ));
            return false;
        }
    }
    for stack in &stacks {
        if !up(node, stack) {
            say(&format!("limen: apply stopped at stack {stack}\n"));
            return false;
        }
    }
    let down: Vec<state::ServiceState> =
        state::services(node).unwrap_or_default().into_iter().filter(|s| !s.running).collect();
    for s in &down {
        say(&format!("limen: expected {} {} is not running\n", s.kind, s.name));
    }
    if down.is_empty() {
        say(&format!("limen: apply finished: {} script(s), {} stack(s)\n", selected.len(), stacks.len()));
    } else {
        say(&format!("limen: apply finished with {} expected service(s) not running\n", down.len()));
    }
    down.is_empty()
}

/// `docker compose up` of one stack, streamed like a script.
fn up(node: &Node, stack: &str) -> bool {
    let file = match state::compose_file(node, stack) {
        Ok(f) => f,
        Err(e) => {
            say(&format!("<== stack {stack}: FAILED ({})\n", e.message));
            return false;
        }
    };
    let Some(docker) = proc::which("docker") else {
        say(&format!("<== stack {stack}: FAILED (docker is not installed)\n"));
        return false;
    };
    say(&format!("==> stack {stack}\n"));
    let mut stream = |_: i32, bytes: &[u8]| sys::out_bytes(bytes);
    let argv: Vec<String> = [docker.as_str(), "compose", "-p", stack, "-f", &file, "up", "-d", "--remove-orphans"]
        .map(String::from)
        .to_vec();
    let r = proc::run(
        &argv,
        proc::Run {
            env: proc::root_env(),
            timeout: Duration::from_secs(1800),
            on_chunk: Some(&mut stream),
            ..Default::default()
        },
    );
    let ok = r.as_ref().is_ok_and(proc::ProcResult::ok);
    let why = match &r {
        Ok(r) => format!("exit {}", r.exit_code),
        Err(e) => e.clone(),
    };
    say(&if ok { format!("<== stack {stack}: ok\n") } else { format!("<== stack {stack}: FAILED ({why})\n") });
    ok
}

pub fn action(node: &Node, name: &str, args: &Map<String, Value>) -> Result<bool> {
    let (entry, spec) = node_scripts::find(node, ScriptKind::Action, name)?;
    // Arguments are checked before anything runs or is logged as run.
    node_scripts::environment(&spec, args)?;
    Ok(run_one(node, &entry, &spec, args))
}

fn run_one(node: &Node, entry: &ScriptEntry, spec: &ScriptSpec, args: &Map<String, Value>) -> bool {
    let env = match node_scripts::environment(spec, args) {
        Ok(env) => env,
        Err(e) => {
            say(&format!("<== {}: FAILED ({})\n", spec.name, e.message));
            return false;
        }
    };
    let now = iso(node.now());
    let log = format!("{}/{}-{}.log", node.config.runs, now.replace(':', ""), spec.name);
    let log_ok = fs::mkdirs(&node.config.runs, 0o700).is_ok()
        && fs::append_line(&log, &format!("# {} {} {now}", spec.kind.name(), spec.name)).is_ok();
    say(&format!("==> {}: {}\n", spec.name, spec.description));
    let mut stream = |_: i32, bytes: &[u8]| {
        sys::out_bytes(bytes);
        if log_ok {
            fs::append(&log, bytes, 0o600).ok();
        }
    };
    let r = node_scripts::run(entry, spec, env, 64 * 1024, Some(&mut stream));
    let (ok, result) = match &r {
        Err(e) => (false, e.message.clone()),
        Ok(r) if r.timed_out => (false, format!("timed out after {}s", spec.timeout_seconds)),
        Ok(r) => match r.signal {
            Some(s) => (false, format!("killed by signal {s}")),
            None => (r.ok(), format!("exit {}", r.exit_code)),
        },
    };
    if log_ok {
        fs::append_line(&log, &format!("# {result}")).ok();
    }
    say(&if ok { format!("<== {}: ok\n", spec.name) } else { format!("<== {}: FAILED ({result})\n", spec.name) });
    ok
}
