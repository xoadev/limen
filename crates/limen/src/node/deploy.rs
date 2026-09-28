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

const STACK_UP_TIMEOUT: Duration = Duration::from_secs(1800);

/// How a step ended: nothing to add when it succeeded, why when it didn't.
type StepOutcome = std::result::Result<(), String>;

pub fn run(node: &Node, request: &str, args: &Map<String, Value>) -> Result<bool> {
    Ok(match request {
        "sync" => sync(node),
        "apply" => {
            let dry_run = args.bool("dry_run").unwrap_or(false);
            let sync_first = args.bool("sync").unwrap_or(true);
            apply(node, args.string("from"), dry_run, sync_first)
        }
        "action" => action(node, args.string("name").unwrap_or_default(), &args.object("args"))?,
        other => unreachable!("not a deploy request: {other}"),
    })
}

pub fn sync(node: &Node) -> bool {
    let Some(repo) = &node.config.repo else {
        sys::say("limen: no [repo] in limen.toml; nothing to sync");
        return false;
    };
    let folder = if repo.path.is_empty() { String::new() } else { format!(" ({})", repo.path) };
    sys::say(&format!("==> sync {} {}{folder}", repo.display_url(), repo.branch));
    match repo::sync(node, repo) {
        Ok((before, after)) => {
            let after_short = repo::short_hash(&after);
            if before.as_deref() == Some(after.as_str()) {
                sys::say(&format!("<== sync: already at {after_short}"));
            } else {
                sys::say(&format!(
                    "<== sync: {} -> {after_short}",
                    before.as_deref().map_or("nothing", repo::short_hash)
                ));
            }
            true
        }
        Err(failure) => finish("sync", Err(failure.message)),
    }
}

/// From the repository to a running node (spec §6.1): sync, the setup scripts in order, the compose stacks node.toml
/// declares, and then `state`, which must say that everything expected runs.
pub fn apply(node: &Node, from: Option<&str>, dry_run: bool, sync_first: bool) -> bool {
    if sync_first && !dry_run && node.config.repo.is_some() && !sync(node) {
        return false;
    }
    let Some(scripts) = setup_scripts(node) else { return false };
    let selected: Vec<(&ScriptEntry, &ScriptSpec)> = scripts
        .iter()
        .filter(|entry| from.is_none_or(|first| at_or_after(&entry.file, first)))
        .map(|entry| (entry, entry.spec.as_ref().expect("setup_scripts keeps only scripts without problems")))
        .collect();
    let stacks = match state::expectations(node) {
        Ok(expected) => expected.compose,
        Err(failure) => {
            sys::say(&format!("limen: apply stopped: {}", failure.message));
            return false;
        }
    };
    if dry_run {
        describe_plan(&selected, &stacks);
        return true;
    }
    run_setup_scripts(node, &selected)
        && bring_up_stacks(node, &stacks)
        && expected_services_run(node, &selected, &stacks)
}

pub fn action(node: &Node, name: &str, args: &Map<String, Value>) -> Result<bool> {
    let (entry, spec) = node_scripts::find(node, ScriptKind::Action, name)?;
    // Arguments are checked before anything runs or is logged as run.
    node_scripts::environment(&spec, args)?;
    Ok(run_script(node, &entry, &spec, args))
}

/// The setup scripts that count, if every one is valid: they are all checked before the first runs, because a broken
/// one halfway would leave the machine half done.
fn setup_scripts(node: &Node) -> Option<Vec<ScriptEntry>> {
    let scripts: Vec<ScriptEntry> =
        node_scripts::discover(node, ScriptKind::Setup).into_iter().filter(|entry| !entry.ignored).collect();
    let problems: Vec<String> =
        scripts.iter().filter_map(|entry| entry.problem.as_ref()).map(|problem| format!("  {problem}")).collect();
    if problems.is_empty() {
        return Some(scripts);
    }
    sys::say(&format!("limen: apply stopped before running anything:\n{}", problems.join("\n")));
    None
}

/// Whether a setup script comes at `--from` or later. Numbers are compared padded to four digits: 5 comes before 20.
fn at_or_after(file: &str, from: &str) -> bool {
    let padded = |number: &str| format!("{number:0>4}");
    padded(number_prefix(file)) >= padded(from)
}

/// `20` of `20-docker`: what `--from` takes.
fn number_prefix(name: &str) -> &str {
    name.split('-').next().unwrap_or("")
}

fn describe_plan(scripts: &[(&ScriptEntry, &ScriptSpec)], stacks: &[String]) {
    for (_, spec) in scripts {
        sys::say(&format!("would run {}: {}", spec.name, spec.description));
    }
    for stack in stacks {
        sys::say(&format!("would bring up stack {stack}"));
    }
}

fn run_setup_scripts(node: &Node, scripts: &[(&ScriptEntry, &ScriptSpec)]) -> bool {
    for (entry, spec) in scripts {
        if !run_script(node, entry, spec, &Map::new()) {
            sys::say(&format!(
                "limen: apply stopped at {}; resume with --from {}",
                spec.name,
                number_prefix(&spec.name)
            ));
            return false;
        }
    }
    true
}

fn bring_up_stacks(node: &Node, stacks: &[String]) -> bool {
    for stack in stacks {
        if !finish(&format!("stack {stack}"), compose_up(node, stack)) {
            sys::say(&format!("limen: apply stopped at stack {stack}"));
            return false;
        }
    }
    true
}

/// `state`'s services after the scripts and stacks: apply succeeds only when everything expected runs.
fn expected_services_run(node: &Node, scripts: &[(&ScriptEntry, &ScriptSpec)], stacks: &[String]) -> bool {
    let down: Vec<state::ServiceState> =
        state::services(node).unwrap_or_default().into_iter().filter(|service| !service.running).collect();
    for service in &down {
        sys::say(&format!("limen: expected {} {} is not running", service.kind, service.name));
    }
    if down.is_empty() {
        sys::say(&format!("limen: apply finished: {} script(s), {} stack(s)", scripts.len(), stacks.len()));
    } else {
        sys::say(&format!("limen: apply finished with {} expected service(s) not running", down.len()));
    }
    down.is_empty()
}

/// `docker compose up` of one stack, streamed like a script.
fn compose_up(node: &Node, stack: &str) -> StepOutcome {
    let file = state::compose_file(node, stack).map_err(|failure| failure.message)?;
    let docker = proc::which("docker").ok_or("docker is not installed")?;
    sys::say(&format!("==> stack {stack}"));
    let argv: Vec<String> = std::iter::once(docker.as_str())
        .chain(state::compose_args(stack, &file, &["up", "-d", "--remove-orphans"]))
        .map(String::from)
        .collect();
    let mut stream = |_: i32, bytes: &[u8]| sys::out_bytes(bytes);
    let options = proc::Run {
        env: proc::root_env(),
        timeout: STACK_UP_TIMEOUT,
        on_chunk: Some(&mut stream),
        ..Default::default()
    };
    let result = proc::run(&argv, options)?;
    if result.ok() { Ok(()) } else { Err(format!("exit {}", result.exit_code)) }
}

fn run_script(node: &Node, entry: &ScriptEntry, spec: &ScriptSpec, args: &Map<String, Value>) -> bool {
    finish(&spec.name, run_logged(node, entry, spec, args))
}

fn run_logged(node: &Node, entry: &ScriptEntry, spec: &ScriptSpec, args: &Map<String, Value>) -> StepOutcome {
    let env = node_scripts::environment(spec, args).map_err(|failure| failure.message)?;
    let log = RunLog::start(node, spec);
    sys::say(&format!("==> {}: {}", spec.name, spec.description));
    let mut stream = |_: i32, bytes: &[u8]| {
        sys::out_bytes(bytes);
        log.append(bytes);
    };
    let result = node_scripts::run(entry, spec, env, node_scripts::MAX_OUTPUT_BYTES, Some(&mut stream));
    let (succeeded, ending) = match &result {
        Err(failure) => (false, failure.message.clone()),
        Ok(done) if done.timed_out => (false, format!("timed out after {}s", spec.timeout_seconds)),
        Ok(done) => match done.signal {
            Some(signal) => (false, format!("killed by signal {signal}")),
            None => (done.ok(), format!("exit {}", done.exit_code)),
        },
    };
    log.end(&ending);
    if succeeded { Ok(()) } else { Err(ending) }
}

/// `runs/<time>-<name>.log`: a script's output between a header and how it ended. A log that can't be written doesn't
/// stop the script.
struct RunLog {
    path: String,
    writable: bool,
}

impl RunLog {
    fn start(node: &Node, spec: &ScriptSpec) -> Self {
        let started = iso(node.now());
        let path = format!("{}/{}-{}.log", node.config.runs, started.replace(':', ""), spec.name);
        let writable = fs::mkdirs(&node.config.runs, 0o700).is_ok()
            && fs::append_line(&path, &format!("# {} {} {started}", spec.kind.name(), spec.name)).is_ok();
        RunLog { path, writable }
    }

    fn append(&self, bytes: &[u8]) {
        if self.writable {
            fs::append(&self.path, bytes, 0o600).ok();
        }
    }

    fn end(&self, ending: &str) {
        if self.writable {
            fs::append_line(&self.path, &format!("# {ending}")).ok();
        }
    }
}

/// A step's closing line, `<== step: ok` or FAILED and why; whether it succeeded.
fn finish(step: &str, outcome: StepOutcome) -> bool {
    match outcome {
        Ok(()) => {
            sys::say(&format!("<== {step}: ok"));
            true
        }
        Err(why) => {
            sys::say(&format!("<== {step}: FAILED ({why})"));
            false
        }
    }
}
