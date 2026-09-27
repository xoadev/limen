//! `node.toml` in the node's folder of the repository (spec §6.1): what must be running. The scripts are how the
//! node gets there; this is what `state` compares against.

use crate::requests::UNIT;
use crate::toml_reader::{self, Reader, TomlResult};
use regex::Regex;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expectations {
    /// Docker Compose stacks: `stacks/<name>/compose.yaml`, brought up by `apply`.
    pub compose: Vec<String>,
    /// systemd units that must be active.
    pub units: Vec<String>,
    /// procd services (OpenWrt) that must exist and not have failed.
    pub procd: Vec<String>,
}

impl Expectations {
    pub fn parse(text: &str) -> TomlResult<Expectations> {
        let table = toml_reader::parse(text)?;
        let root = Reader::new(&table);
        let mut out = Expectations::default();
        if let Some(expect) = root.table("expect")? {
            out.compose = names(&expect, "compose", "^[a-z0-9][a-z0-9_-]{0,62}$")?;
            out.units = names(&expect, "units", UNIT)?;
            out.procd = names(&expect, "procd", "^[A-Za-z0-9._-]{1,64}$")?;
            expect.reject_unknown()?;
        }
        root.reject_unknown()?;
        Ok(out)
    }
}

fn names(t: &Reader, key: &str, shape: &str) -> TomlResult<Vec<String>> {
    let list = t.strings(key)?.unwrap_or_default();
    let shape = Regex::new(shape).unwrap();
    if let Some(bad) = list.iter().find(|n| !shape.is_match(n)) {
        return t.fail(key, &format!("'{bad}' is not a valid name"));
    }
    Ok(list)
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
