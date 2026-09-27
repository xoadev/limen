//! Arguments of requests and scripts (spec §5, §6). The same [`Param`] validates on the node and becomes the JSON
//! Schema of the MCP tool on the hub, so the two can't disagree.

use crate::protocol::{Result, bad_request};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Int,
    Bool,
    Enum,
    String,
    /// Only for the nested arguments of `check` and `action`, which the script's own header validates.
    Object,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ParamType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// What a script's string argument accepts when its header says nothing (spec §6).
pub const DEFAULT_STRING_PATTERN: &str = "^[A-Za-z0-9._-]{1,64}$";

/// An argument's own name.
pub const PARAM_NAME: &str = "^[a-z][a-z0-9_]{0,31}$";

impl Param {
    pub fn new(name: &str, kind: ParamType, description: &str) -> Self {
        Self {
            name: name.into(),
            kind,
            description: description.into(),
            required: false,
            default: None,
            min: None,
            max: None,
            values: None,
            pattern: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    pub fn range(mut self, min: Option<i64>, max: Option<i64>) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    pub fn values(mut self, values: &[&str]) -> Self {
        self.values = Some(values.iter().map(|v| v.to_string()).collect());
        self
    }

    pub fn pattern(mut self, pattern: &str) -> Self {
        self.pattern = Some(pattern.into());
        self
    }

    pub fn json_schema(&self) -> Value {
        let mut s = Map::new();
        match self.kind {
            ParamType::Int => {
                s.insert("type".into(), json!("integer"));
                if let Some(min) = self.min {
                    s.insert("minimum".into(), json!(min));
                }
                if let Some(max) = self.max {
                    s.insert("maximum".into(), json!(max));
                }
            }
            ParamType::Bool => {
                s.insert("type".into(), json!("boolean"));
            }
            ParamType::Enum => {
                s.insert("type".into(), json!("string"));
                s.insert("enum".into(), json!(self.values.clone().unwrap_or_default()));
            }
            ParamType::String => {
                s.insert("type".into(), json!("string"));
                if let Some(p) = &self.pattern {
                    s.insert("pattern".into(), json!(p));
                }
            }
            ParamType::Object => {
                s.insert("type".into(), json!("object"));
            }
        }
        if !self.description.is_empty() {
            s.insert("description".into(), json!(self.description));
        }
        if let Some(d) = &self.default {
            s.insert("default".into(), d.clone());
        }
        Value::Object(s)
    }

    /// Ok if [value] fits this parameter; otherwise a `bad_request` that says why.
    pub fn check(&self, value: &Value) -> Result<()> {
        let name = &self.name;
        match self.kind {
            ParamType::Int => {
                let n = value.as_i64().ok_or_else(|| bad_request(format!("{name} must be an integer")))?;
                if let Some(min) = self.min.filter(|min| n < *min) {
                    return Err(bad_request(format!("{name} must be at least {min}")));
                }
                if let Some(max) = self.max.filter(|max| n > *max) {
                    return Err(bad_request(format!("{name} must be at most {max}")));
                }
            }
            ParamType::Bool => {
                value.as_bool().ok_or_else(|| bad_request(format!("{name} must be true or false")))?;
            }
            ParamType::Enum => {
                let s = value.as_str().ok_or_else(|| bad_request(format!("{name} must be a string")))?;
                let values = self.values.clone().unwrap_or_default();
                if !values.iter().any(|v| v == s) {
                    return Err(bad_request(format!("{name} must be one of {}", values.join(", "))));
                }
            }
            ParamType::String => {
                let s = value.as_str().ok_or_else(|| bad_request(format!("{name} must be a string")))?;
                if let Some(p) = &self.pattern {
                    let matches = full_match(p).map(|r| r.is_match(s)).unwrap_or(false);
                    if !matches {
                        return Err(bad_request(format!("{name} does not match {p}")));
                    }
                }
            }
            ParamType::Object => {
                if !value.is_object() {
                    return Err(bad_request(format!("{name} must be an object")));
                }
            }
        }
        Ok(())
    }
}

/// [pattern] as a regex that must match the whole text, as a schema `pattern` is read. Compiled once per process.
pub fn full_match(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap().get(pattern) {
        return Ok(r.clone());
    }
    // Bounded repetitions like `{1,4096}` unroll: room for them, the patterns being ours or root's.
    let r = RegexBuilder::new(&format!("^(?:{pattern})$")).size_limit(64 << 20).build()?;
    cache.lock().unwrap().insert(pattern.into(), r.clone());
    Ok(r)
}

/// [args] checked against [params]: unknown names and missing required ones are rejected, defaults filled in. The
/// result only has names from [params].
pub fn validate(params: &[Param], args: &Map<String, Value>) -> Result<Map<String, Value>> {
    if let Some(unknown) = args.keys().find(|k| !params.iter().any(|p| &p.name == *k)) {
        return Err(bad_request(format!("unknown argument '{unknown}'")));
    }
    let mut out = Map::new();
    for p in params {
        match args.get(&p.name).filter(|v| !v.is_null()) {
            Some(given) => {
                p.check(given)?;
                out.insert(p.name.clone(), given.clone());
            }
            None => {
                if let Some(d) = &p.default {
                    out.insert(p.name.clone(), d.clone());
                } else if p.required {
                    return Err(bad_request(format!("missing argument '{}'", p.name)));
                }
            }
        }
    }
    Ok(out)
}

/// The JSON Schema of a tool: [extra] (the hub's own arguments, with whether they are required) then [params].
pub fn input_schema(params: &[Param], extra: &[(Param, bool)]) -> Value {
    let mut properties = Map::new();
    for p in extra.iter().map(|(p, _)| p).chain(params) {
        properties.insert(p.name.clone(), p.json_schema());
    }
    let required: Vec<&str> = extra
        .iter()
        .filter(|(_, r)| *r)
        .map(|(p, _)| p.name.as_str())
        .chain(params.iter().filter(|p| p.required).map(|p| p.name.as_str()))
        .collect();
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    Value::Object(schema)
}

/// Typed reads of validated arguments.
pub trait ArgsExt {
    fn string(&self, name: &str) -> Option<&str>;
    fn long(&self, name: &str) -> Option<i64>;
    /// An integer that must fit an `i32`: one that doesn't is refused, not wrapped.
    fn int(&self, name: &str) -> Result<Option<i32>>;
    fn bool(&self, name: &str) -> Option<bool>;
    fn obj(&self, name: &str) -> Map<String, Value>;
}

impl ArgsExt for Map<String, Value> {
    fn string(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    fn long(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(Value::as_i64)
    }

    fn int(&self, name: &str) -> Result<Option<i32>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| bad_request(format!("{name} is out of range"))),
        }
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(Value::as_bool)
    }

    fn obj(&self, name: &str) -> Map<String, Value> {
        self.get(name).and_then(Value::as_object).cloned().unwrap_or_default()
    }
}
