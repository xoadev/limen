//! systemd: units through `systemctl`, the log through `journalctl`, each asked for JSON or a plain listing.

use super::init::{InitSystem, LogFilter};
use super::read::{borrowed, last_matching_messages, matches_whole, owned, scan_window};
use super::{Answer, MINUTE, Node, internal};
use limen_core::params::ArgsExt;
use limen_core::protocol::{ErrorCode, Result, bad_request, error};
use limen_core::requests::UNIT;
use limen_core::system::parsers;
use limen_core::time::iso;
use serde_json::{Map, Value, json};

type Args = Map<String, Value>;

pub struct Systemd;

impl InitSystem for Systemd {
    fn units(&self, node: &Node, args: &Args) -> Result<Value> {
        units(node, args)
    }

    fn service(&self, node: &Node, name: &str, lines: usize) -> Result<Value> {
        service(node, name, lines)
    }

    fn failed(&self, node: &Node) -> Result<Vec<String>> {
        failed_units(node)
    }

    fn log(&self, node: &Node, lines: usize, filter: &LogFilter) -> Result<Answer> {
        journal(node, lines, filter)
    }
}

fn units(node: &Node, args: &Args) -> Result<Value> {
    let mut argv = owned(&["systemctl", "list-units", "--no-legend", "--plain", "--no-pager"]);
    if let Some(unit_type) = args.string("type").filter(|unit_type| *unit_type != "all") {
        argv.push(format!("--type={unit_type}"));
    }
    match args.string("state") {
        None => {}
        Some("all") => argv.push("--all".into()),
        Some("inactive") => argv.extend(owned(&["--all", "--state=inactive"])),
        Some(state) => argv.push(format!("--state={state}")),
    }
    if let Some(pattern) = args.string("pattern") {
        argv.extend(owned(&["--", pattern]));
    }
    Ok(parsers::units(&node.exec_ok(&borrowed(&argv))?, &node.redactor))
}

/// A unit (a service when [name] says no type) and its last [lines] in the journal.
fn service(node: &Node, name: &str, lines: usize) -> Result<Value> {
    let unit = if name.contains('.') { name.to_string() } else { format!("{name}.service") };
    let properties = parsers::key_values(&node.exec_ok(&[
        "systemctl",
        "show",
        "--no-pager",
        "--property=Id,Description,LoadState,ActiveState,SubState,Result,UnitFileState,FragmentPath,MainPID,\
         ExecMainStatus,NRestarts,MemoryCurrent,ActiveEnterTimestamp,StateChangeTimestamp,Type,Restart",
        "--",
        &unit,
    ])?);
    if properties.get("LoadState").map(String::as_str) == Some("not-found") {
        return Err(error(ErrorCode::NotFound, format!("no unit named {unit}")));
    }
    let journal = unit_journal(node, &unit, lines)?;
    let mut service = parsers::unit(&properties, &node.redactor);
    service["journal"] = json!(journal);
    Ok(service)
}

fn unit_journal(node: &Node, unit: &str, lines: usize) -> Result<Vec<Value>> {
    if lines == 0 {
        return Ok(vec![]);
    }
    let count = lines.to_string();
    let journal = node.exec_ok(&["journalctl", "-u", unit, "-n", &count, "-o", "json", "--no-pager", "-q"])?;
    Ok(parsers::journal(&journal, &node.redactor))
}

fn failed_units(node: &Node) -> Result<Vec<String>> {
    let listed = node.exec_ok(&["systemctl", "list-units", "--failed", "--no-legend", "--plain", "--no-pager"])?;
    Ok(parsers::units(&listed, &node.redactor)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|unit| unit["unit"].as_str().map(String::from))
        .collect())
}

/// The journal, of the unit [filter] names as its source, or of the whole system.
fn journal(node: &Node, lines: usize, filter: &LogFilter) -> Result<Answer> {
    let argv = journalctl_argv(node, lines, filter)?;
    let result = node.exec(&borrowed(&argv), MINUTE)?;
    // journalctl --grep exits 1 when nothing matches: that is an empty answer, not an error.
    let output = result.out();
    if result.exit_code != 0 && output.trim().is_empty() && !result.err().trim().is_empty() {
        return Err(internal(format!("journalctl: {}", result.failure_reason())));
    }
    let entries = parsers::journal(&output, &node.redactor);
    Ok(Answer::cut(json!(last_matching_messages(entries, filter.grep, lines)), result.truncated))
}

fn journalctl_argv(node: &Node, lines: usize, filter: &LogFilter) -> Result<Vec<String>> {
    let mut argv = owned(&["journalctl", "-o", "json", "--no-pager", "-q", "-n"]);
    argv.push(scan_window(node, lines, filter.grep).to_string());
    if let Some(unit) = filter.source {
        if !matches_whole(UNIT, unit) {
            return Err(bad_request(format!("'{unit}' is not a unit name")));
        }
        argv.extend(owned(&["-u", unit]));
    }
    if let Some(since) = filter.since {
        argv.push(format!("--since={}", journal_time(since)));
    }
    if let Some(until) = filter.until {
        argv.push(format!("--until={}", journal_time(until)));
    }
    if let Some(priority) = filter.priority {
        argv.push(format!("--priority={priority}"));
    }
    if let Some(grep) = filter.grep {
        argv.extend([format!("--grep={}", pcre_literal(grep)), "--case-sensitive=false".into()]);
    }
    Ok(argv)
}

fn journal_time(epoch: i64) -> String {
    format!("{} UTC", iso(epoch).replace('T', " ").trim_end_matches('Z'))
}

/// [text] as a PCRE pattern that matches only itself: every non-alphanumeric character escaped.
fn pcre_literal(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_alphanumeric() || character == ' ' {
                character.to_string()
            } else {
                format!("\\{character}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_for_journalctl_grep() {
        assert_eq!(pcre_literal("a.b (c)"), "a\\.b \\(c\\)");
    }
}
