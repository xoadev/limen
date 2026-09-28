//! `node.toml` in the node's folder of the repository (spec §6.1): what must be running. The scripts are how the
//! node gets there; this is what `state` compares against.

use super::{ConfigResult, fail};
use crate::own_regex;
use crate::requests::UNIT;
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
        let expectations = super::from_str::<File>(text)?.expect;
        for (key, names, shape) in [
            ("expect.compose", &expectations.compose, "^[a-z0-9][a-z0-9_-]{0,62}$"),
            ("expect.units", &expectations.units, UNIT),
            ("expect.procd", &expectations.procd, "^[A-Za-z0-9._-]{1,64}$"),
        ] {
            let shape = own_regex(shape);
            if let Some(bad) = names.iter().find(|name| !shape.is_match(name)) {
                return fail(key, format!("'{bad}' is not a valid name"));
            }
        }
        Ok(expectations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checks_names() {
        assert_eq!(Expectations::parse("").unwrap(), Expectations::default());
        let expectations = Expectations::parse(
            "[expect]\ncompose = [\"immich\"]\nunits = [\"docker.service\"]\nprocd = [\"dnsmasq\"]",
        )
        .unwrap();
        assert_eq!(expectations.compose, ["immich"]);
        assert_eq!(expectations.units, ["docker.service"]);
        assert!(Expectations::parse("[expect]\ncompose = [\"../x\"]").is_err());
        assert!(Expectations::parse("[expect]\nservices = []").is_err());
    }
}
