//! Arguments of requests and scripts (spec §5, §6). The same [`Param`] validates on the node and becomes the JSON
//! Schema of the MCP tool on the hub, so the two can't disagree.

use crate::protocol::{LimenError, Result, bad_request};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Int,
    Bool,
    Enum,
    String,
    /// Only for the nested arguments of `run`, which the script's own header validates.
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
/// Not an option (`-x`), not `.` or `..`: a script may put it in a command line or a path.
pub const DEFAULT_STRING_PATTERN: &str = "^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$";

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
        self.values = Some(values.iter().map(ToString::to_string).collect());
        self
    }

    pub fn pattern(mut self, pattern: &str) -> Self {
        self.pattern = Some(pattern.into());
        self
    }

    pub fn json_schema(&self) -> Value {
        let mut schema = Map::new();
        match self.kind {
            ParamType::Int => {
                schema.insert("type".into(), json!("integer"));
                if let Some(min) = self.min {
                    schema.insert("minimum".into(), json!(min));
                }
                if let Some(max) = self.max {
                    schema.insert("maximum".into(), json!(max));
                }
            }
            ParamType::Bool => {
                schema.insert("type".into(), json!("boolean"));
            }
            ParamType::Enum => {
                schema.insert("type".into(), json!("string"));
                schema.insert("enum".into(), json!(self.allowed_values()));
            }
            ParamType::String => {
                schema.insert("type".into(), json!("string"));
                if let Some(pattern) = &self.pattern {
                    schema.insert("pattern".into(), json!(pattern));
                }
            }
            ParamType::Object => {
                schema.insert("type".into(), json!("object"));
            }
        }
        if !self.description.is_empty() {
            schema.insert("description".into(), json!(self.description));
        }
        if let Some(default) = &self.default {
            schema.insert("default".into(), default.clone());
        }
        Value::Object(schema)
    }

    /// Ok if [value] fits this parameter; otherwise a `bad_request` that says why.
    pub fn check(&self, value: &Value) -> Result<()> {
        match self.kind {
            ParamType::Int => self.check_int(value),
            ParamType::Bool if !value.is_boolean() => Err(self.refusal("must be true or false")),
            ParamType::Enum => self.check_enum(value),
            ParamType::String => self.check_string(value),
            ParamType::Object if !value.is_object() => Err(self.refusal("must be an object")),
            ParamType::Bool | ParamType::Object => Ok(()),
        }
    }

    fn check_int(&self, value: &Value) -> Result<()> {
        let number = value.as_i64().ok_or_else(|| self.refusal("must be an integer"))?;
        if let Some(min) = self.min.filter(|min| number < *min) {
            return Err(self.refusal(&format!("must be at least {min}")));
        }
        if let Some(max) = self.max.filter(|max| number > *max) {
            return Err(self.refusal(&format!("must be at most {max}")));
        }
        Ok(())
    }

    fn check_enum(&self, value: &Value) -> Result<()> {
        let text = self.text_of(value)?;
        let allowed = self.allowed_values();
        if !allowed.iter().any(|name| name == text) {
            return Err(self.refusal(&format!("must be one of {}", allowed.join(", "))));
        }
        Ok(())
    }

    fn check_string(&self, value: &Value) -> Result<()> {
        let text = self.text_of(value)?;
        // Before the pattern: a permissive one such as `.{0,200}` would let a NUL through to fail at exec.
        if text.chars().any(char::is_control) {
            return Err(self.refusal("must not contain control characters"));
        }
        if let Some(pattern) = &self.pattern {
            if !full_match(pattern).is_ok_and(|regex| regex.is_match(text)) {
                return Err(self.refusal(&format!("does not match {pattern}")));
            }
        }
        Ok(())
    }

    fn text_of<'a>(&self, value: &'a Value) -> Result<&'a str> {
        value.as_str().ok_or_else(|| self.refusal("must be a string"))
    }

    fn allowed_values(&self) -> &[String] {
        self.values.as_deref().unwrap_or_default()
    }

    /// A `bad_request` that says [why], after this argument's name, its value doesn't fit.
    fn refusal(&self, why: &str) -> LimenError {
        bad_request(format!("{} {why}", self.name))
    }
}

/// Bounded repetitions like `{1,4096}` unroll: room for them, the patterns being ours or root's.
const REGEX_SIZE_LIMIT: usize = 64 << 20;

const CACHE_LOCK: &str = "nothing panics while holding the regex cache";

/// [pattern] as a regex that must match the whole text, as a schema `pattern` is read. Compiled once per process.
pub fn full_match(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    static COMPILED: LazyLock<Mutex<HashMap<String, Regex>>> = LazyLock::new(Default::default);
    if let Some(regex) = COMPILED.lock().expect(CACHE_LOCK).get(pattern) {
        return Ok(regex.clone());
    }
    let regex = RegexBuilder::new(&format!("^(?:{pattern})$")).size_limit(REGEX_SIZE_LIMIT).build()?;
    COMPILED.lock().expect(CACHE_LOCK).insert(pattern.into(), regex.clone());
    Ok(regex)
}

/// [args] checked against [params]: unknown names and missing required ones are rejected, defaults filled in. The
/// result only has names from [params].
pub fn validate(params: &[Param], args: &Map<String, Value>) -> Result<Map<String, Value>> {
    if let Some(unknown) = args.keys().find(|name| !params.iter().any(|param| &param.name == *name)) {
        return Err(bad_request(format!("unknown argument '{unknown}'")));
    }
    let mut valid = Map::new();
    for param in params {
        match args.get(&param.name).filter(|value| !value.is_null()) {
            Some(given) => {
                param.check(given)?;
                valid.insert(param.name.clone(), given.clone());
            }
            None => {
                if let Some(default) = &param.default {
                    valid.insert(param.name.clone(), default.clone());
                } else if param.required {
                    return Err(bad_request(format!("missing argument '{}'", param.name)));
                }
            }
        }
    }
    Ok(valid)
}

/// The JSON Schema of a tool: [extra] (the hub's own arguments, with whether they are required) then [params].
pub fn input_schema(params: &[Param], extra: &[(Param, bool)]) -> Value {
    let mut properties = Map::new();
    for param in extra.iter().map(|(param, _)| param).chain(params) {
        properties.insert(param.name.clone(), param.json_schema());
    }
    let required: Vec<&str> = extra
        .iter()
        .filter(|(_, required)| *required)
        .map(|(param, _)| param)
        .chain(params.iter().filter(|param| param.required))
        .map(|param| param.name.as_str())
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
    fn integer(&self, name: &str) -> Option<i64>;
    /// An integer that must fit an `i32`: one that doesn't is refused, not wrapped.
    fn small_integer(&self, name: &str) -> Result<Option<i32>>;
    fn bool(&self, name: &str) -> Option<bool>;
    fn object(&self, name: &str) -> Map<String, Value>;
}

impl ArgsExt for Map<String, Value> {
    fn string(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    fn integer(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(Value::as_i64)
    }

    fn small_integer(&self, name: &str) -> Result<Option<i32>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| bad_request(format!("{name} is out of range"))),
        }
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(Value::as_bool)
    }

    fn object(&self, name: &str) -> Map<String, Value> {
        self.get(name).and_then(Value::as_object).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive() -> Param {
        Param::new("text", ParamType::String, "").pattern("^.{0,200}$")
    }

    fn without_pattern() -> Param {
        Param::new("text", ParamType::String, "")
    }

    #[test]
    fn a_string_with_a_control_character_is_refused_before_its_pattern() {
        for text in ["a\0b", "a\nb", "a\tb", "a\rb", "\u{1b}[31m", "a\u{7f}", "a\u{9b}31m"] {
            for param in [permissive(), without_pattern()] {
                let refusal = param.check(&json!(text)).expect_err(text);
                assert_eq!(refusal.code, crate::protocol::ErrorCode::BadRequest, "{text:?}");
                assert_eq!(refusal.message, "text must not contain control characters", "{text:?}");
            }
        }
    }

    #[test]
    fn a_string_without_control_characters_still_meets_its_pattern() {
        assert!(permissive().check(&json!("café ñandú 日本 🙂")).is_ok());
        assert!(permissive().check(&json!("")).is_ok());
        let narrow = Param::new("text", ParamType::String, "").pattern("^[a-z]+$");
        assert_eq!(narrow.check(&json!("ABC")).expect_err("no match").message, "text does not match ^[a-z]+$");
    }

    #[test]
    fn validate_refuses_a_nul_in_an_argument() {
        let params = [permissive().required()];
        let args = json!({"text": "ok\0"}).as_object().unwrap().clone();
        let refusal = validate(&params, &args).expect_err("refused");
        assert_eq!(refusal.message, "text must not contain control characters");
    }
}
