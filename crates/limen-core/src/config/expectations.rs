//! `node.toml` in the node's folder of the repository (spec §6.1): what must be running. The scripts are how the
//! node gets there; this is what `state` compares against.

use super::{ConfigResult, fail};
use crate::requests::UNIT;
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Expectations {
    /// Docker Compose stacks: `stacks/<name>/compose.yaml`, brought up by `apply`.
    pub compose: Vec<String>,
    /// systemd units that must be active.
    pub units: Vec<String>,
    /// procd services (OpenWrt) that must exist and not have failed.
    pub procd: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    expect: Expectations,
}

impl Expectations {
    pub fn parse(text: &str) -> ConfigResult<Expectations> {
        let e = super::from_str::<File>(text)?.expect;
        for (key, names, shape) in [
            ("expect.compose", &e.compose, "^[a-z0-9][a-z0-9_-]{0,62}$"),
            ("expect.units", &e.units, UNIT),
            ("expect.procd", &e.procd, "^[A-Za-z0-9._-]{1,64}$"),
        ] {
            let shape = Regex::new(shape).expect("limen's own patterns compile");
            if let Some(bad) = names.iter().find(|n| !shape.is_match(n)) {
                return fail(key, format!("'{bad}' is not a valid name"));
            }
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checks_names() {
        assert_eq!(Expectations::parse("").unwrap(), Expectations::default());
        let e = Expectations::parse(
            "[expect]\ncompose = [\"immich\"]\nunits = [\"docker.service\"]\nprocd = [\"dnsmasq\"]",
        )
        .unwrap();
        assert_eq!(e.compose, ["immich"]);
        assert_eq!(e.units, ["docker.service"]);
        assert!(Expectations::parse("[expect]\ncompose = [\"../x\"]").is_err());
        assert!(Expectations::parse("[expect]\nservices = []").is_err());
    }
}
