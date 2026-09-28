//! limen's rules (spec §12): protocol, request schemas, argument validation, configuration, script headers, path
//! policy, redaction and parsers. Pure: no processes, no files, no network, so every rule is tested without a machine.

use regex::Regex;

pub mod config;
pub mod durations;
pub mod glob;
pub mod join;
pub mod params;
pub mod path_policy;
pub mod protocol;
pub mod redactor;
pub mod requests;
pub mod scripts;
pub mod system;
pub mod time;
pub mod trust;
pub mod version;

/// One of limen's own regular expressions: a constant of this code, never input, so it compiles.
pub fn own_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("limen's own patterns compile")
}
