//! `limen lint` (spec §10): every script directory checked without running anything. Exit 1 when something is wrong.

use super::{Node, scripts};
use crate::os::sys;
use limen_core::scripts::ScriptKind;
use std::collections::BTreeMap;

pub fn run(node: &Node) -> i32 {
    let mut problems = 0;
    for kind in ScriptKind::ALL {
        let dir = node.config.directory(kind);
        let entries = scripts::discover(node, kind);
        sys::out(&format!("{dir}: {} file(s)\n", entries.len()));
        // disk.sh and disk.py are both `disk`: which one runs would depend on the order of the directory.
        let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &entries {
            if let Some(spec) = &e.spec {
                by_name.entry(spec.name.as_str()).or_default().push(e.file.as_str());
            }
        }
        for (name, files) in by_name.iter().filter(|(_, f)| f.len() > 1) {
            problems += 1;
            sys::out(&format!("  FAIL {} are both '{name}'; keep one\n", files.join(" and ")));
        }
        for e in &entries {
            match (&e.spec, &e.problem) {
                (_, Some(p)) if e.ignored => sys::out(&format!("  skip {p}\n")),
                (_, Some(p)) => {
                    problems += 1;
                    sys::out(&format!("  FAIL {p}\n"));
                }
                (Some(s), None) => sys::out(&format!(
                    "  ok   {}: {} ({} argument(s), timeout {}s)\n",
                    s.name,
                    s.description,
                    s.params.len(),
                    s.timeout_seconds
                )),
                (None, None) => {}
            }
        }
    }
    sys::out(if problems == 0 { "lint: OK\n".into() } else { format!("lint: {problems} problem(s)\n") }.as_str());
    i32::from(problems != 0)
}
