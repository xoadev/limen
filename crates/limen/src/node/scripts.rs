//! The scripts of the node's packs (spec §6, docs/scripts.md): found, validated and run.

use super::{Answer, Node, gate, internal};
use crate::os::{fs, proc, sys};
use limen_core::params::{self, ArgsExt};
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::scripts::{self, Catalog, ScriptSpec};
use limen_core::trust;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

type Args = Map<String, Value>;

const MAX_HEADER: usize = 64 * 1024;
/// What a script may print, both streams together, before it is stopped.
pub const MAX_OUTPUT_BYTES: usize = 16 << 20;
/// What a request is given on top of its script's own timeout: starting it, stopping it, answering.
const MARGIN: Duration = Duration::from_secs(30);

/// A file of a pack: its spec when it is a usable script, what is wrong with it otherwise, or [ignored] when it is not
/// a script at all —a README, a helper without a header—, which only `limen lint` mentions.
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    pub pack: String,
    pub file: String,
    pub path: String,
    pub spec: Option<ScriptSpec>,
    pub problem: Option<String>,
    pub ignored: bool,
}

impl ScriptEntry {
    fn usable(pack: &str, file: &str, resolved: &str, spec: ScriptSpec) -> Self {
        let entry = ScriptEntry::broken(pack, file, resolved, String::new());
        ScriptEntry { spec: Some(spec), problem: None, ..entry }
    }

    fn broken(pack: &str, file: &str, path: &str, problem: String) -> Self {
        ScriptEntry {
            pack: pack.into(),
            file: file.into(),
            path: path.into(),
            spec: None,
            problem: Some(problem),
            ignored: false,
        }
    }

    fn not_a_script(pack: &str, file: &str, path: &str, why: &str) -> Self {
        ScriptEntry { ignored: true, ..ScriptEntry::broken(pack, file, path, format!("{path}: {why}")) }
    }
}

/// Every file at the top of every pack but hidden ones (a `.gitkeep`), each with its spec or what it is.
pub fn discover(node: &Node) -> Vec<ScriptEntry> {
    node.config.packs.iter().flat_map(|pack| discover_pack(node, pack)).collect()
}

fn discover_pack(node: &Node, pack: &str) -> Vec<ScriptEntry> {
    match fs::stat(pack) {
        None => return vec![ScriptEntry::broken(pack, "", pack, format!("{pack}: the pack does not exist"))],
        Some(info) if info.kind != fs::FileType::Directory => {
            return vec![ScriptEntry::broken(pack, "", pack, format!("{pack}: the pack is not a directory"))];
        }
        Some(_) => {}
    }
    fs::list(pack)
        .unwrap_or_default()
        .into_iter()
        .filter(|file| !file.starts_with('.'))
        .filter_map(|file| read_entry(node, pack, &file))
        .collect()
}

/// [file] of [pack] as an entry; none for a subdirectory, which is the pack's own business.
fn read_entry(node: &Node, pack: &str, file: &str) -> Option<ScriptEntry> {
    let path = format!("{pack}/{file}");
    let info = fs::stat(&path)?;
    if info.kind == fs::FileType::Directory {
        return None;
    }
    if info.kind != fs::FileType::File {
        return Some(ScriptEntry::not_a_script(pack, file, &path, "not a regular file; ignored"));
    }
    let Some(name) = scripts::name_of(file) else {
        return Some(ScriptEntry::not_a_script(pack, file, &path, "not a script name (a-z, 0-9, - and _); ignored"));
    };
    Some(match trusted_spec(node, &name, &path) {
        Ok(Some((resolved, spec))) => ScriptEntry::usable(pack, file, &resolved, spec),
        Ok(None) => ScriptEntry::not_a_script(pack, file, &path, "no `#:` header: a helper, not a script"),
        Err(problem) => ScriptEntry::broken(pack, file, &path, format!("{path}: {problem}")),
    })
}

/// The script at [path], resolved, with its header parsed and trusted; none when it has no header —a helper, or the
/// node's own `limen.toml` in its folder—, executable or not; what stops it otherwise. Reading a header runs nothing,
/// so it comes first: only a file that says it is a script must meet the rules for running one.
fn trusted_spec(node: &Node, name: &str, path: &str) -> std::result::Result<Option<(String, ScriptSpec)>, String> {
    let resolved = fs::real_path(path).ok_or("cannot resolve")?;
    let header = fs::read(&resolved, MAX_HEADER).ok_or("cannot read")?;
    let Some(spec) = scripts::parse(name, &String::from_utf8_lossy(&header)) else { return Ok(None) };
    if let Some(problem) = trust::problem(&fs::chain(&resolved), node.trusted_owner) {
        return Err(problem);
    }
    spec.map(|spec| Some((resolved, spec)))
}

pub fn catalog(node: &Node) -> Catalog {
    let entries = discover(node);
    let mut problems: Vec<String> =
        entries.iter().filter(|entry| !entry.ignored).filter_map(|entry| entry.problem.clone()).collect();
    let mut scripts = Vec::new();
    for (name, named) in by_name(&entries) {
        match named.as_slice() {
            [entry] => scripts.extend(entry.spec.clone()),
            _ => problems.push(same_name_problem(name, &named)),
        }
    }
    Catalog { scripts, problems }
}

/// The usable scripts by name: a name with more than one is refused, all of them, until one goes.
pub fn by_name(entries: &[ScriptEntry]) -> BTreeMap<&str, Vec<&ScriptEntry>> {
    let mut named: BTreeMap<&str, Vec<&ScriptEntry>> = BTreeMap::new();
    for entry in entries {
        if let Some(spec) = &entry.spec {
            named.entry(spec.name.as_str()).or_default().push(entry);
        }
    }
    named
}

/// Two files that are both the script [name]: which one ran would depend on the order of the packs.
pub fn same_name_problem(name: &str, entries: &[&ScriptEntry]) -> String {
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    format!("{} are both '{name}'; keep one", paths.join(" and "))
}

/// The usable script [name]; `not_found` or `unavailable` with the reason otherwise.
pub fn find(node: &Node, name: &str) -> Result<(ScriptEntry, ScriptSpec)> {
    let entries = discover(node);
    if let Some(named) = by_name(&entries).get(name).filter(|named| named.len() > 1) {
        return Err(error(ErrorCode::Unavailable, same_name_problem(name, named)));
    }
    let Some(entry) =
        entries.into_iter().find(|entry| !entry.ignored && scripts::name_of(&entry.file).as_deref() == Some(name))
    else {
        return Err(error(ErrorCode::NotFound, format!("no script named '{name}' in this node's packs")));
    };
    let spec = entry.spec.clone().ok_or_else(|| {
        error(ErrorCode::Unavailable, entry.problem.clone().unwrap_or_else(|| "unusable script".into()))
    })?;
    Ok((entry, spec))
}

/// `run`: the script, its arguments checked against its header; its output redacted, then filtered.
pub fn run(node: &Node, args: &Args) -> Result<Answer> {
    let (entry, spec) = find(node, args.string("script").unwrap_or_default())?;
    let env = environment(&entry, &spec, &args.object("args"))?;
    let timeout = Duration::from_secs(spec.timeout_seconds);
    gate::allow(timeout + MARGIN);
    let result = proc::run(
        std::slice::from_ref(&entry.path),
        proc::Run { env, timeout, max_output: MAX_OUTPUT_BYTES, ..Default::default() },
    )
    .map_err(internal)?;
    if result.timed_out {
        return Err(error(ErrorCode::Timeout, format!("{} did not finish in {}s", spec.name, spec.timeout_seconds)));
    }
    let tail = args.small_integer("tail")?.map(|tail| tail.max(1) as usize);
    let (stdout, stdout_cut) = node.filter(&node.redactor.redact(&result.out()), args.string("grep"), tail);
    let (stderr, stderr_cut) = node.filter(&node.redactor.redact(&result.err()), None, None);
    let mut answer = json!({"script": spec.name, "exit": result.exit_code});
    if let Some(signal) = result.signal {
        answer["signal"] = json!(signal);
    }
    answer["stdout"] = json!(stdout.join("\n"));
    answer["stderr"] = json!(stderr.join("\n"));
    Ok(Answer::cut(answer, result.truncated || stdout_cut || stderr_cut))
}

/// The environment of a script: a clean one plus `LIMEN_*` (docs/scripts.md). Arguments are validated first.
fn environment(entry: &ScriptEntry, spec: &ScriptSpec, args: &Args) -> Result<Vec<String>> {
    let values = params::validate(&spec.params, args)?;
    let mut env = proc::root_env();
    env.push(format!("LIMEN_SCRIPT={}", spec.name));
    env.push(format!("LIMEN_PACK={}", entry.pack));
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

#[cfg(test)]
mod tests {
    use super::*;
    use limen_core::config::node::NodeConfig;
    use std::os::unix::fs::PermissionsExt;

    /// Two packs, removed at the end.
    struct Packs {
        dir: String,
        node: Node,
    }

    impl Drop for Packs {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// Where every directory up to `/` is root's or the user's and nobody else writes: not /tmp, not a checkout made
    /// with umask 002. A script under a directory others can write is never run.
    fn packs(files: &[(&str, &str)]) -> Packs {
        let base = ["XDG_RUNTIME_DIR", "HOME"].into_iter().find_map(std::env::var_os).expect("a private directory");
        let dir = format!("{}/limen-packs-{}", base.to_string_lossy(), crate::hub::dir::random(8).unwrap());
        for subdir in ["", "/one", "/one/lib", "/two"] {
            let path = format!("{dir}{subdir}");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for (file, text) in files {
            let path = format!("{dir}/{file}");
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = NodeConfig { packs: vec![format!("{dir}/one"), format!("{dir}/two")], ..Default::default() };
        Packs { node: Node::new(config), dir }
    }

    const ECHO: &str = "#!/bin/sh\n#: description = \"Says it\"\n#: [args.word]\n#: type = \"string\"\n\
                        echo \"$LIMEN_ARG_WORD from $(basename \"$LIMEN_PACK\")\"\necho password=hunter2\necho oops >&2\n\
                        exit 3\n";

    fn run_args(script: &str, args: &Value, filters: &Value) -> Args {
        let mut request = json!({"script": script, "args": args});
        request.as_object_mut().unwrap().extend(filters.as_object().unwrap().clone());
        request.as_object().unwrap().clone()
    }

    #[test]
    fn the_packs_scripts_and_what_is_not_one() {
        let packs = packs(&[
            ("one/say.sh", ECHO),
            ("one/README.md", "# the pack\n"),
            ("one/helper", "#!/bin/sh\necho help\n"),
            ("one/limen.toml", "[files]\nallow = []\n"),
            ("one/forgotten", "#!/bin/sh\n#: description = \"Not made executable\"\n"),
            ("one/lib/format.sh", "#: description = \"inside a subdirectory\"\n"),
            ("two/broken", "#: timeout = \"1s\"\n"),
        ]);
        // Not executable: the node's own limen.toml, next to its scripts, and a script whose chmod was forgotten.
        for file in ["one/limen.toml", "one/forgotten"] {
            std::fs::set_permissions(format!("{}/{file}", packs.dir), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let catalog = catalog(&packs.node);
        assert_eq!(catalog.scripts.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(), ["say"]);
        assert_eq!(catalog.problems.len(), 2, "{:?}", catalog.problems);
        assert!(catalog.problems[0].contains("forgotten is not executable"), "{:?}", catalog.problems);
        assert!(catalog.problems[1].contains("broken: the header has no description"), "{:?}", catalog.problems);
    }

    #[test]
    fn a_name_in_two_packs_is_refused_in_both() {
        let packs = packs(&[("one/say.sh", ECHO), ("two/say", ECHO)]);
        assert!(catalog(&packs.node).scripts.is_empty());
        let refused = find(&packs.node, "say").expect_err("refused");
        assert_eq!(refused.code, ErrorCode::Unavailable);
        assert!(refused.message.contains("are both 'say'"), "{}", refused.message);
    }

    #[test]
    fn runs_with_its_arguments_and_answers_redacted_and_filtered() {
        let packs = packs(&[("one/say.sh", ECHO)]);
        let answer = run(&packs.node, &run_args("say", &json!({"word": "hi"}), &json!({}))).unwrap();
        assert_eq!(answer.data["exit"], 3);
        assert_eq!(answer.data["stdout"], "hi from one\npassword=[redacted]");
        assert_eq!(answer.data["stderr"], "oops");
        let filtered = run(&packs.node, &run_args("say", &json!({"word": "hi"}), &json!({"grep": "=h"}))).unwrap();
        assert_eq!(filtered.data["stdout"], "", "a guess at the secret is not told apart");
        let last = run(&packs.node, &run_args("say", &json!({"word": "hi"}), &json!({"tail": 1}))).unwrap();
        assert_eq!(last.data["stdout"], "password=[redacted]");
    }

    #[test]
    fn arguments_the_header_does_not_take_never_run_it() {
        let packs = packs(&[("one/say.sh", ECHO)]);
        for args in [json!({}), json!({"word": "-rf"}), json!({"word": "hi", "other": 1})] {
            let refused = run(&packs.node, &run_args("say", &args, &json!({}))).err().expect("refused");
            assert_eq!(refused.code, ErrorCode::BadRequest, "{args}");
        }
        assert_eq!(
            run(&packs.node, &run_args("nope", &json!({}), &json!({}))).err().unwrap().code,
            ErrorCode::NotFound
        );
    }
}
