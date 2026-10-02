//! The scripts of a node's packs (spec §6, docs/scripts.md): what a file's name and `#:` header say about it. The
//! header is parsed, never run: learning what a script does must not run it.

use crate::config;
use crate::durations;
use crate::own_regex;
use crate::params::{self, DEFAULT_STRING_PATTERN, PARAM_NAME, Param, ParamType};
use crate::requests::{self, RESERVED_ARGS, SCRIPT_NAME};
use indexmap::IndexMap;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// The longest description of a script or an argument the hub takes: that text reaches every MCP session.
pub const MAX_DESCRIPTION_CHARS: usize = 300;
/// The longest `pattern` the hub takes, and the most it may compile to: the hub compiles the patterns of nodes it
/// doesn't trust.
pub const MAX_PATTERN_BYTES: usize = 512;
const MAX_COMPILED_PATTERN: usize = 1 << 20;

/// What a script says about itself in its header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptSpec {
    pub name: String,
    pub description: String,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub params: Vec<Param>,
    /// The header's word that the script changes nothing: MCP clients may run it without asking. A node that predates
    /// it sends none, and its scripts read as ones that may change things.
    #[serde(default)]
    pub read_only: bool,
}

/// The scripts of a node, and what is wrong with the ones that could not be read. Part of `hello`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub scripts: Vec<ScriptSpec>,
    #[serde(default)]
    pub problems: Vec<String>,
}

static SCRIPT_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| own_regex(SCRIPT_NAME));

static PARAM_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| own_regex(PARAM_NAME));

pub fn is_script_name(name: &str) -> bool {
    SCRIPT_NAME_REGEX.is_match(name)
}

pub fn is_param_name(name: &str) -> bool {
    PARAM_NAME_REGEX.is_match(name)
}

/// Whether [name] is one of the hub's own tools or the node's requests: a script with it would take its place.
pub fn is_reserved_name(name: &str) -> bool {
    name == "nodes" || requests::find(name).is_some()
}

/// Why the hub leaves [spec] out of its tools, or None when it takes it. A node's catalog is not trusted, and what
/// becomes a tool's name, schema or description reaches every MCP session: the hub takes only what limen itself would
/// write, bounded. `limen lint` says the same of a node's own scripts.
pub fn hub_refusal(spec: &ScriptSpec) -> Option<String> {
    let name = &spec.name;
    if !is_script_name(name) {
        return Some(format!("{name:?} is not a script's name"));
    }
    if is_reserved_name(name) {
        return Some(format!("{name}: the hub has a tool or request of that name"));
    }
    if !(1..=MAX_TIMEOUT.as_secs()).contains(&spec.timeout_seconds) {
        return Some(format!("{name}: timeout is 1s to 1h"));
    }
    let described = std::iter::once(("the description".to_string(), &spec.description))
        .chain(spec.params.iter().map(|param| (format!("the description of '{}'", param.name), &param.description)));
    described
        .into_iter()
        .find_map(|(what, text)| text_refusal(&what, text))
        .or_else(|| spec.params.iter().find_map(param_refusal))
        .map(|why| format!("{name}: {why}"))
}

/// Why the hub doesn't take [text] as a description: too long, or with control characters.
fn text_refusal(what: &str, text: &str) -> Option<String> {
    let length = text.chars().count();
    if length > MAX_DESCRIPTION_CHARS {
        return Some(format!("{what} has {length} characters and the hub takes {MAX_DESCRIPTION_CHARS}"));
    }
    text.chars().any(char::is_control).then(|| format!("{what} holds a control character"))
}

/// Why the hub doesn't take [param]: a name that isn't an argument's, or limen's own, or a pattern too big for it.
fn param_refusal(param: &Param) -> Option<String> {
    let name = &param.name;
    if !is_param_name(name) {
        return Some(format!("argument name {name:?} must match {PARAM_NAME}"));
    }
    if RESERVED_ARGS.contains(&name.as_str()) {
        return Some(format!("argument name '{name}' is limen's own"));
    }
    let pattern = param.pattern.as_deref()?;
    if pattern.len() > MAX_PATTERN_BYTES {
        return Some(format!(
            "the pattern of '{name}' has {} bytes and the hub takes {MAX_PATTERN_BYTES}",
            pattern.len()
        ));
    }
    let compiles = RegexBuilder::new(pattern).size_limit(MAX_COMPILED_PATTERN).build().is_ok();
    (!compiles).then(|| format!("the pattern of '{name}' compiles to more than 1 MiB"))
}

/// The script name of a file: its name without the extension, or None when that is not a script's name.
pub fn name_of(file: &str) -> Option<String> {
    if file.starts_with('.') {
        return None;
    }
    let base = file.rsplit_once('.').map_or(file, |(base, _extension)| base);
    is_script_name(base).then(|| base.into())
}

/// The `#:` lines of the leading comment block, without the marker: a TOML document.
pub fn extract(text: &str) -> Option<String> {
    let mut lines = text.lines().peekable();
    lines.next_if(|line| line.starts_with("#!"));
    let header: Vec<&str> = lines
        .take_while(|line| line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("#:"))
        .map(|content| content.strip_prefix(' ').unwrap_or(content))
        .collect();
    (!header.is_empty()).then(|| header.iter().map(|line| format!("{line}\n")).collect())
}

/// The header as written; [ScriptSpec] is what it means.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    description: Option<String>,
    timeout: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    args: IndexMap<String, Arg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arg {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    description: String,
    range: Option<Vec<i64>>,
    values: Option<Vec<String>>,
    pattern: Option<String>,
    default: Option<Value>,
    required: Option<bool>,
}

/// The script [name] from its [text]; None when it has no header, which makes it a helper of its pack and not a script.
pub fn parse(name: &str, text: &str) -> Option<Result<ScriptSpec, String>> {
    extract(text).map(|header| spec(name, &header))
}

fn spec(name: &str, header_text: &str) -> Result<ScriptSpec, String> {
    let header: Header = config::from_str(header_text).map_err(|error| format!("{name}: header: {error}"))?;
    let description = header.description.ok_or(format!("{name}: the header has no description"))?;
    let timeout = match header.timeout {
        Some(timeout) => {
            durations::parse(&timeout).ok_or(format!("{name}: timeout '{timeout}' is not a duration like 30s or 5m"))?
        }
        None => DEFAULT_TIMEOUT,
    };
    if timeout > MAX_TIMEOUT {
        return Err(format!("{name}: timeout is at most 1h"));
    }
    let params =
        header.args.into_iter().map(|(arg_name, arg)| param(name, &arg_name, arg)).collect::<Result<_, _>>()?;
    Ok(ScriptSpec {
        name: name.into(),
        description,
        timeout_seconds: timeout.as_secs().max(1),
        params,
        read_only: header.read_only,
    })
}

fn param(script: &str, name: &str, arg: Arg) -> Result<Param, String> {
    if !is_param_name(name) {
        return Err(format!("{script}: argument name '{name}' must match {PARAM_NAME}"));
    }
    if RESERVED_ARGS.contains(&name) {
        return Err(format!("{script}: argument name '{name}' is limen's own"));
    }
    arg.into_param(name).map_err(|why| format!("{script}: argument '{name}': {why}"))
}

impl Arg {
    /// What the argument means, or what is wrong with it.
    fn into_param(self, name: &str) -> Result<Param, String> {
        let kind = self.param_type()?;
        self.check_keys_belong_to(kind)?;
        let bounds = self.bounds()?;
        if kind == ParamType::Enum && self.values.as_ref().is_none_or(Vec::is_empty) {
            return Err("is an enum without values".into());
        }
        let pattern =
            (kind == ParamType::String).then(|| self.pattern.unwrap_or_else(|| DEFAULT_STRING_PATTERN.into()));
        if pattern.as_deref().is_some_and(|pattern| params::full_match(pattern).is_err()) {
            return Err("bad pattern".into());
        }
        if self.default.as_ref().is_some_and(|default| !fits_a_type(default)) {
            return Err("default must be a string, integer or boolean".into());
        }
        // The schema would say required while the node fills the default in: the model can't tell which holds.
        if self.required == Some(true) && self.default.is_some() {
            return Err("required = true and a default: the default makes it optional".into());
        }
        let param = Param {
            name: name.into(),
            kind,
            description: self.description,
            required: self.required.unwrap_or(self.default.is_none()),
            default: self.default,
            min: bounds.map(|(min, _)| min),
            max: bounds.map(|(_, max)| max),
            values: self.values,
            pattern,
        };
        if let Some(default) = &param.default {
            param.check(default).map_err(|error| format!("the default does not fit ({error})"))?;
        }
        Ok(param)
    }

    fn param_type(&self) -> Result<ParamType, String> {
        match self.kind.as_deref() {
            Some("int") => Ok(ParamType::Int),
            Some("bool") => Ok(ParamType::Bool),
            Some("enum") => Ok(ParamType::Enum),
            Some("string") => Ok(ParamType::String),
            None => Err("has no type".into()),
            Some(other) => Err(format!("has type '{other}'; it is int, bool, enum or string")),
        }
    }

    /// Each key belongs to one type: `range` on a string is a mistake to say, not to ignore.
    fn check_keys_belong_to(&self, kind: ParamType) -> Result<(), String> {
        for (key, given, belongs_to) in [
            ("range", self.range.is_some(), ParamType::Int),
            ("values", self.values.is_some(), ParamType::Enum),
            ("pattern", self.pattern.is_some(), ParamType::String),
        ] {
            if given && kind != belongs_to {
                return Err(format!("{key} does not apply to this type"));
            }
        }
        Ok(())
    }

    /// `range = [min, max]` as the pair it is.
    fn bounds(&self) -> Result<Option<(i64, i64)>, String> {
        match self.range.as_deref() {
            None => Ok(None),
            Some([min, max]) if min <= max => Ok(Some((*min, *max))),
            Some(_) => Err("range is [min, max]".into()),
        }
    }
}

/// Whether a header's `default` can be the value of some argument type: TOML's floats, dates, arrays and tables can't.
fn fits_a_type(value: &Value) -> bool {
    value.is_string() || value.is_i64() || value.is_boolean()
}

/// `threshold` → `LIMEN_ARG_THRESHOLD`: how an argument reaches the script.
pub fn env_name(param: &str) -> String {
    format!("LIMEN_ARG_{}", param.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCRIPT: &str = r#"#!/usr/bin/env bash
#: description = "Free space on the backup volume"
#: timeout = "30s"
#: [args.threshold]
#: type = "int"
#: default = 90
#: range = [1, 100]
#: [args.mount]
#: type = "string"
#: pattern = "^[A-Za-z0-9._-]{1,64}$"
set -euo pipefail
#: description = "not part of the header"
"#;

    #[test]
    fn parses_the_header() {
        let spec = parse("backup-space", SCRIPT).unwrap().unwrap();
        assert_eq!(spec.description, "Free space on the backup volume");
        assert_eq!(spec.timeout_seconds, 30);
        let threshold = spec.params.iter().find(|param| param.name == "threshold").unwrap();
        assert_eq!(threshold.default, Some(json!(90)));
        assert_eq!(threshold.min, Some(1));
        assert!(!threshold.required);
        let mount = spec.params.iter().find(|param| param.name == "mount").unwrap();
        assert!(mount.required);
        assert!(!spec.read_only, "a script may change the machine unless its header says otherwise");
    }

    #[test]
    fn read_only_is_what_the_header_says() {
        let read_only = parse("a", "#: description = \"x\"\n#: read_only = true\n").unwrap().unwrap();
        assert!(read_only.read_only);
        let changes = parse("a", "#: description = \"x\"\n#: read_only = false\n").unwrap().unwrap();
        assert!(!changes.read_only);
        let word = parse("a", "#: description = \"x\"\n#: read_only = \"yes\"\n").unwrap().unwrap_err();
        assert!(word.contains("line 2") && word.contains("expected a boolean"), "{word}");
    }

    #[test]
    fn a_catalog_from_a_node_without_read_only_reads_as_changing() {
        let spec: ScriptSpec =
            serde_json::from_value(json!({"name": "a", "description": "x", "timeout_seconds": 60})).unwrap();
        assert!(!spec.read_only);
    }

    #[test]
    fn a_description_over_the_limit_is_told_by_script_and_argument() {
        let fits = "x".repeat(MAX_DESCRIPTION_CHARS);
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        let script = |description: &str, arg_description: &str| {
            format!(
                "#!/bin/sh\n#: description = \"{description}\"\n#: [args.word]\n#: type = \"string\"\n#: description = \"{arg_description}\"\n"
            )
        };
        let spec = parse("say", &script(&fits, &fits)).unwrap().unwrap();
        assert_eq!(hub_refusal(&spec), None);
        let spec = parse("say", &script(&long, &fits)).unwrap().unwrap();
        assert_eq!(hub_refusal(&spec).unwrap(), "say: the description has 301 characters and the hub takes 300");
        let spec = parse("say", &script(&fits, &long)).unwrap().unwrap();
        assert_eq!(
            hub_refusal(&spec).unwrap(),
            "say: the description of 'word' has 301 characters and the hub takes 300"
        );
    }

    #[test]
    fn the_hub_leaves_out_a_script_that_would_take_one_of_its_names() {
        let named = |name: &str| ScriptSpec {
            name: name.into(),
            description: "x".into(),
            timeout_seconds: 60,
            params: vec![],
            read_only: false,
        };
        assert_eq!(hub_refusal(&named("disk")), None);
        for reserved in ["nodes", "hello", "read_file", "list_dir", "history", "run"] {
            assert_eq!(
                hub_refusal(&named(reserved)).unwrap(),
                format!("{reserved}: the hub has a tool or request of that name")
            );
        }
    }

    #[test]
    fn the_hub_leaves_out_text_and_patterns_it_would_not_write() {
        let with_pattern = |pattern: &str| ScriptSpec {
            name: "say".into(),
            description: "x".into(),
            timeout_seconds: 60,
            params: vec![Param::new("word", ParamType::String, "").pattern(pattern)],
            read_only: false,
        };
        assert_eq!(hub_refusal(&with_pattern(&format!("^{}$", "a".repeat(MAX_PATTERN_BYTES - 2)))), None);
        assert_eq!(
            hub_refusal(&with_pattern(&format!("^{}$", "a".repeat(MAX_PATTERN_BYTES - 1)))).unwrap(),
            "say: the pattern of 'word' has 513 bytes and the hub takes 512"
        );
        assert_eq!(hub_refusal(&with_pattern("^[A-Za-z0-9_]{1,300}$")), None);
        let unrolled = r"^\w{1,300}$";
        assert!(params::full_match(unrolled).is_ok(), "the node itself takes it");
        assert_eq!(
            hub_refusal(&with_pattern(unrolled)).unwrap(),
            "say: the pattern of 'word' compiles to more than 1 MiB"
        );
        let bell = ScriptSpec { description: "ring\u{7}".into(), ..with_pattern("^a$") };
        assert_eq!(hub_refusal(&bell).unwrap(), "say: the description holds a control character");
    }

    #[test]
    fn a_minute_by_default_an_hour_at_most() {
        let text = "#!/bin/sh\n#: description = \"x\"\n";
        assert_eq!(parse("a", text).unwrap().unwrap().timeout_seconds, 60);
        let long = "#: description = \"x\"\n#: timeout = \"2h\"\n";
        assert!(parse("a", long).unwrap().unwrap_err().contains("at most 1h"));
    }

    #[test]
    fn a_file_without_a_header_is_a_helper() {
        assert!(parse("lib", "#!/bin/sh\necho hi\n").is_none());
    }

    #[test]
    fn rejects_broken_headers() {
        for (text, expected) in [
            ("#: timeout = \"1s\"", "no description"),
            ("#: description = \"x\"\n#: colour = 1", "unknown field `colour`"),
            ("#: description = \"x\"\n#: [args.n]\n#: type = \"string\"\n#: range = [1, 2]", "range does not apply"),
            ("#: description = \"x\"\n#: timeout = \"soon\"", "not a duration"),
            ("#: description = \"x\"\n#: [args.n]\n#: type = \"float\"", "has type 'float'"),
            ("#: description = \"x\"\n#: [args.n]\n#: type = \"int\"\n#: range = [5, 1]", "range is [min, max]"),
            ("#: description = \"x\"\n#: [args.n]\n#: type = \"enum\"", "enum without values"),
            ("#: description = \"x\"\n#: [args.n]\n#: type = \"int\"\n#: default = \"x\"", "the default does not fit"),
            ("#: description = \"x\"\n#: [args.Bad]\n#: type = \"int\"", "argument name 'Bad'"),
            ("#: description = \"x\"\n#: [args.grep]\n#: type = \"int\"", "'grep' is limen's own"),
            (
                "#: description = \"x\"\n#: [args.n]\n#: type = \"int\"\n#: default = 1\n#: required = true",
                "argument 'n': required = true and a default: the default makes it optional",
            ),
        ] {
            let error = parse("s", text).unwrap().unwrap_err();
            assert!(error.contains(expected), "{error} should contain {expected}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(name_of("disk.sh").as_deref(), Some("disk"));
        assert_eq!(name_of("backup-space").as_deref(), Some("backup-space"));
        assert_eq!(name_of("Disk.sh"), None);
        assert_eq!(name_of(".hidden"), None);
        assert_eq!(env_name("threshold"), "LIMEN_ARG_THRESHOLD");
    }
}
