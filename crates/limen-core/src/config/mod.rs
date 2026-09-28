//! Configuration files (spec §7): the node's and the hub's, each a `#[derive(Deserialize)]` with
//! `deny_unknown_fields`, so a typo fails instead of being ignored.

pub mod hub;
pub mod node;

use serde::de::DeserializeOwned;
use std::fmt;

/// A configuration that is wrong, and where: `line 3, column 1: unknown field …` or `files.allow: …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

pub type ConfigResult<T> = Result<T, ConfigError>;

/// [key] is wrong, for [why].
pub fn fail<T>(key: &str, why: impl fmt::Display) -> ConfigResult<T> {
    Err(ConfigError(format!("{key}: {why}")))
}

/// [text] as a [T], or where and why it isn't one, on one line.
pub fn from_str<T: DeserializeOwned>(text: &str) -> ConfigResult<T> {
    toml_edit::de::from_str(text).map_err(|error| {
        let message = error.message().trim().replace('\n', "; ");
        ConfigError(match error.span() {
            Some(span) => {
                let before = &text[..span.start.min(text.len())];
                let line = before.matches('\n').count() + 1;
                let column = before.len() - before.rfind('\n').map_or(0, |newline| newline + 1) + 1;
                format!("line {line}, column {column}: {message}")
            }
            None => message,
        })
    })
}

/// [value] if it is a positive number, as a count; [key] names it in the error.
fn positive(key: &str, value: Option<i64>) -> ConfigResult<Option<usize>> {
    match value {
        Some(count) if count <= 0 => fail(key, "must be positive"),
        count => Ok(count.map(|count| count as usize)),
    }
}

/// [value] if it is an absolute path.
fn absolute(key: &str, value: Option<String>) -> ConfigResult<Option<String>> {
    match value {
        Some(path) if !path.starts_with('/') => fail(key, "must be an absolute path"),
        path => Ok(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
    }

    #[test]
    fn errors_say_where_on_one_line() {
        assert_eq!(from_str::<Sample>("name = \"a\"").unwrap().name, "a");
        let error = from_str::<Sample>("name = \"a\"\n\ntypo = 1").unwrap_err();
        assert!(error.0.starts_with("line 3, column 1: unknown field `typo`"), "{error}");
        assert!(!error.0.contains('\n'));
        assert!(from_str::<Sample>("name = ").is_err());
    }
}
