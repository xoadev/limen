//! `limen lint` (spec §10): every script directory checked without running anything. Exit 1 when something is wrong.

use super::Node;
use super::scripts::{self, ScriptEntry};
use crate::os::sys;
use limen_core::scripts::ScriptKind;
use std::collections::BTreeMap;

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
    let problems: usize = ScriptKind::ALL.into_iter().map(|kind| lint_directory(node, kind)).sum();
    let verdict = if problems == 0 { "lint: OK\n".to_string() } else { format!("lint: {problems} problem(s)\n") };
    sys::out(&verdict);
    i32::from(problems != 0)
}

/// Reports the directory of [kind] file by file; how many problems it holds.
fn lint_directory(node: &Node, kind: ScriptKind) -> usize {
    let entries = scripts::discover(node, kind);
    sys::out(&format!("{}: {} file(s)\n", node.config.directory(kind), entries.len()));
    let findings = same_names(&entries).into_iter().chain(entries.iter().filter_map(entry_finding));
    let mut problems = 0;
    for finding in findings {
        problems += usize::from(matches!(finding, Finding::Failed(_)));
        sys::out(&finding.line());
    }
    problems
}

/// disk.sh and disk.py are both `disk`: which one runs would depend on the order of the directory.
fn same_names(entries: &[ScriptEntry]) -> Vec<Finding> {
    let mut files_by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in entries {
        if let Some(spec) = &entry.spec {
            files_by_name.entry(spec.name.as_str()).or_default().push(entry.file.as_str());
        }
    }
    files_by_name
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(name, files)| Finding::Failed(scripts::same_name_problem(name, &files)))
        .collect()
}

fn entry_finding(entry: &ScriptEntry) -> Option<Finding> {
    match (&entry.spec, &entry.problem) {
        (_, Some(problem)) if entry.ignored => Some(Finding::Skipped(problem.clone())),
        (_, Some(problem)) => Some(Finding::Failed(problem.clone())),
        (Some(spec), None) => Some(Finding::Usable(format!(
            "{}: {} ({} argument(s), timeout {}s)",
            spec.name,
            spec.description,
            spec.params.len(),
            spec.timeout_seconds
        ))),
        (None, None) => None,
    }
}
