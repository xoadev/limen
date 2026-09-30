//! `limen lint` (spec §10): every pack checked without running anything. Exit 1 when something is wrong.

use super::Node;
use super::scripts::{self, ScriptEntry};
use crate::os::sys;
use limen_core::scripts::too_long_description;

/// A line of the report: a script that would run, a file that is not a script, or a problem.
enum Finding {
    Usable(String),
    Skipped(String),
    Failed(String),
}

impl Finding {
    fn line(&self) -> String {
        match self {
            Finding::Usable(text) => format!("  ok   {text}\n"),
            Finding::Skipped(text) => format!("  skip {text}\n"),
            Finding::Failed(text) => format!("  FAIL {text}\n"),
        }
    }
}

pub fn run(node: &Node) -> i32 {
    let entries = scripts::discover(node);
    if node.config.packs.is_empty() {
        sys::say("no packs in scripts.packs: this node offers no scripts");
    }
    let mut problems = 0;
    for pack in &node.config.packs {
        let in_pack: Vec<&ScriptEntry> = entries.iter().filter(|entry| &entry.pack == pack).collect();
        sys::say(&format!("{pack}: {} file(s)", in_pack.iter().filter(|entry| !entry.file.is_empty()).count()));
        for finding in in_pack.into_iter().filter_map(entry_finding) {
            problems += usize::from(matches!(finding, Finding::Failed(_)));
            sys::out(&finding.line());
        }
    }
    for (name, named) in scripts::by_name(&entries).into_iter().filter(|(_, named)| named.len() > 1) {
        problems += 1;
        sys::out(&Finding::Failed(scripts::same_name_problem(name, &named)).line());
    }
    let verdict = if problems == 0 { "lint: OK\n".to_string() } else { format!("lint: {problems} problem(s)\n") };
    sys::out(&verdict);
    i32::from(problems != 0)
}

fn entry_finding(entry: &ScriptEntry) -> Option<Finding> {
    match (&entry.spec, &entry.problem) {
        (_, Some(problem)) if entry.ignored => Some(Finding::Skipped(problem.clone())),
        (_, Some(problem)) => Some(Finding::Failed(problem.clone())),
        (Some(spec), None) => Some(match too_long_description(spec) {
            Some(problem) => Finding::Failed(problem),
            None => Finding::Usable(format!(
                "{}: {} ({} argument(s), timeout {}s)",
                spec.name,
                spec.description,
                spec.params.len(),
                spec.timeout_seconds
            )),
        }),
        (None, None) => None,
    }
}
