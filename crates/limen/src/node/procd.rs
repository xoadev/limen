//! OpenWrt's procd: services through `ubus`, the log through `logread`, filtered here because busybox's can't.

use super::init::{InitSystem, LogFilter};
use super::read::last_matching_messages;
use super::{Answer, MINUTE, Node};
use crate::os::fs;
use limen_core::glob::segment_matches;
use limen_core::params::ArgsExt;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::redactor::Redactor;
use limen_core::system::parsers;
use limen_core::system::procfs::{self, LogreadEntry};
use limen_core::time;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

type Args = Map<String, Value>;

pub struct Procd;

impl InitSystem for Procd {
    fn units(&self, node: &Node, args: &Args) -> Result<Value> {
        units(node, args.string("state"), args.string("pattern"))
    }

    fn service(&self, node: &Node, name: &str, lines: usize) -> Result<Value> {
        service(node, name.trim_end_matches(".service"), lines)
    }

    fn failed(&self, node: &Node) -> Result<Vec<String>> {
        Ok(services(node, None)?
            .into_iter()
            .filter(|(_, service)| service.state == ProcdState::Failed)
            .map(|(name, _)| name)
            .collect())
    }

    /// `source = unit` names a program, as procd's services are named.
    fn log(&self, node: &Node, lines: usize, filter: &LogFilter) -> Result<Answer> {
        let program = filter.source.map(|unit| unit.trim_end_matches(".service"));
        let entries = logread(node, lines, &LogFilter { source: program, ..*filter })?;
        Ok(Answer::of(json!(entries)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcdState {
    Running,
    Configured,
    Failed,
}

impl ProcdState {
    /// Running when an instance runs, failed when none of them does, configured when it has none.
    fn of(instances: &Map<String, Value>) -> Self {
        let running = |instance: &Value| instance.get("running").and_then(Value::as_bool) == Some(true);
        if instances.is_empty() {
            ProcdState::Configured
        } else if instances.values().any(running) {
            ProcdState::Running
        } else {
            ProcdState::Failed
        }
    }

    /// As systemd's `ActiveState` would say it.
    fn active(self) -> &'static str {
        if self == ProcdState::Failed { "failed" } else { "active" }
    }

    /// As systemd's `SubState` would say it.
    fn sub(self) -> &'static str {
        match self {
            ProcdState::Running => "running",
            ProcdState::Configured => "exited",
            ProcdState::Failed => "dead",
        }
    }

    /// Whether `services` asked for `state = [state_filter]` lists a service in this state.
    fn matches(self, state_filter: Option<&str>) -> bool {
        match state_filter {
            None | Some("all") => true,
            Some("running") => self == ProcdState::Running,
            Some("failed") => self == ProcdState::Failed,
            Some("active") => self != ProcdState::Failed,
            Some("exited") => self == ProcdState::Configured,
            _ => false,
        }
    }
}

pub struct ProcdService {
    pub state: ProcdState,
    pub instances: Map<String, Value>,
}

impl ProcdService {
    fn from_ubus(service: &Value) -> Self {
        let instances = service.get("instances").and_then(Value::as_object).cloned().unwrap_or_default();
        ProcdService { state: ProcdState::of(&instances), instances }
    }
}

/// `ubus call service list`: every procd service and its instances, or only [name]'s.
pub fn services(node: &Node, name: Option<&str>) -> Result<BTreeMap<String, ProcdService>> {
    let query = match name {
        None => "{}".to_string(),
        Some(name) => json!({"name": name, "verbose": true}).to_string(),
    };
    let listed = node.exec_ok(&["ubus", "call", "service", "list", &query])?;
    // Nothing printed, or nothing readable, is no service.
    let services: Map<String, Value> = serde_json::from_str(&listed).unwrap_or_default();
    Ok(services.into_iter().map(|(name, service)| (name, ProcdService::from_ubus(&service))).collect())
}

/// Whether `/etc/rc.d` starts [name] at boot, as `service <name> enabled` says on OpenWrt.
pub fn enabled(name: &str) -> bool {
    starts_at_boot(&fs::list("/etc/rc.d").unwrap_or_default(), name)
}

/// `S<digits><name>` among [links], exactly.
fn starts_at_boot(links: &[String], name: &str) -> bool {
    links.iter().any(|link| {
        link.strip_prefix('S').is_some_and(|rest| {
            let service = rest.trim_start_matches(|character: char| character.is_ascii_digit());
            service.len() < rest.len() && service == name
        })
    })
}

fn units(node: &Node, state: Option<&str>, pattern: Option<&str>) -> Result<Value> {
    let rows = services(node, None)?
        .into_iter()
        .filter(|(name, _)| pattern.is_none_or(|pattern| segment_matches(pattern, name)))
        .filter(|(_, service)| service.state.matches(state))
        .map(|(name, service)| unit_row(&name, &service))
        .collect();
    Ok(Value::Array(rows))
}

/// A procd service as `services` lists a systemd unit.
fn unit_row(name: &str, service: &ProcdService) -> Value {
    json!({
        "unit": name,
        "load": "loaded",
        "active": service.state.active(),
        "sub": service.state.sub(),
        "enabled": enabled(name),
        "instances": service.instances.len(),
    })
}

fn service(node: &Node, name: &str, lines: usize) -> Result<Value> {
    let service = match services(node, Some(name))?.remove(name) {
        Some(service) => service,
        None if fs::exists(&format!("/etc/init.d/{name}")) => {
            ProcdService { state: ProcdState::Configured, instances: Map::new() }
        }
        None => return Err(error(ErrorCode::NotFound, format!("no service named {name}"))),
    };
    let instances: Vec<Value> = service
        .instances
        .iter()
        .filter_map(|(instance, details)| instance_row(&node.redactor, instance, details))
        .collect();
    let journal =
        if lines > 0 { logread(node, lines, &LogFilter { source: Some(name), ..Default::default() })? } else { vec![] };
    Ok(json!({
        "name": name,
        "active": service.state.active(),
        "sub": service.state.sub(),
        "enabled": enabled(name),
        "instances": instances,
        "journal": journal,
    }))
}

fn instance_row(redactor: &Redactor, name: &str, details: &Value) -> Option<Value> {
    let fields = details.as_object()?;
    let command = fields
        .get("command")
        .and_then(Value::as_array)
        .map(|words| redactor.redact(&words.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")));
    Some(json!({
        "name": name,
        "running": fields.get("running").cloned().unwrap_or(json!(false)),
        "pid": fields.get("pid").and_then(Value::as_i64),
        "exit_code": fields.get("exit_code").and_then(Value::as_i64),
        "command": command,
    }))
}

/// The OpenWrt log (`logread`), filtered here: by source (the program, as `unit` names it), by priority, by time and
/// by grep. The last [lines] that match, oldest first.
fn logread(node: &Node, lines: usize, filter: &LogFilter) -> Result<Vec<Value>> {
    let window = if filter.narrows() { node.config.scan_lines } else { lines };
    let log = logread_tail(node, window)?;
    let max_level = filter.priority.and_then(parsers::priority_number);
    let rows: Vec<Value> = log
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| logread_row(&node.redactor, line, filter, max_level))
        .collect();
    Ok(last_matching_messages(rows, filter.grep, lines))
}

fn logread_tail(node: &Node, lines: usize) -> Result<String> {
    let count = lines.to_string();
    let result = node.exec(&["logread", "-l", &count], MINUTE)?;
    if result.exit_code != 0 {
        return Err(error(ErrorCode::Unavailable, format!("logread: {}", result.failure_reason())));
    }
    Ok(result.out())
}

/// [line] as a row, if [filter] lets it through. A line not in logread's shape has no fields to filter by: it goes
/// through only when nothing but grep filters.
fn logread_row(redactor: &Redactor, line: &str, filter: &LogFilter, max_level: Option<usize>) -> Option<Value> {
    let Some(entry) = procfs::logread_line(line) else {
        let unfiltered =
            filter.source.is_none() && max_level.is_none() && filter.since.is_none() && filter.until.is_none();
        return unfiltered.then(|| json!({"message": redactor.redact(line)}));
    };
    passes(&entry, filter, max_level).then(|| {
        json!({
            "time": entry.time,
            "priority": entry.priority,
            "source": entry.source,
            "pid": entry.pid,
            "message": redactor.redact(&entry.message),
        })
    })
}

/// Whether [entry] comes from the filter's source (or from `<source>-…`), is at [max_level] or more urgent, and falls
/// in its time span.
fn passes(entry: &LogreadEntry, filter: &LogFilter, max_level: Option<usize>) -> bool {
    let from_source = |source: &str| entry.source == source || entry.source.starts_with(&format!("{source}-"));
    let urgent_enough = |max: usize| parsers::priority_number(&entry.priority).is_some_and(|level| level <= max);
    filter.source.is_none_or(from_source)
        && max_level.is_none_or(urgent_enough)
        && filter.spans(time::parse_iso(&entry.time))
}

#[cfg(test)]
mod tests {
    #[test]
    fn enabled_is_an_rc_d_link_by_exact_name() {
        let links: Vec<String> =
            ["S19dnsmasq", "S19dnsmasq-extra", "K10firewall", "Sfoo"].iter().map(|link| link.to_string()).collect();
        assert!(super::starts_at_boot(&links, "dnsmasq"));
        assert!(!super::starts_at_boot(&links, "dns"));
        assert!(!super::starts_at_boot(&links, "firewall"));
        assert!(!super::starts_at_boot(&links, "foo"));
    }
}
