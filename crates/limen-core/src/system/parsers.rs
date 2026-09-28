//! What the gate reads from the programs it runs, turned into the JSON it answers.

use crate::redactor::Redactor;
use crate::time::iso;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// `systemctl show` output: one `Key=Value` per line.
pub fn key_values(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// `systemctl list-units --no-legend --plain`: UNIT LOAD ACTIVE SUB DESCRIPTION. A description is whatever the unit
/// file says, so it is redacted like any other text.
pub fn units(text: &str, redactor: &Redactor) -> Value {
    let rows: Vec<Value> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts = split_whitespace(line, 5);
            let description = redactor.redact(parts.get(4).copied().unwrap_or(""));
            match parts[..] {
                [unit, load, active, sub, ..] => {
                    Some(json!({"unit": unit, "load": load, "active": active, "sub": sub, "description": description}))
                }
                _ => None,
            }
        })
        .collect();
    Value::Array(rows)
}

/// [line] split on runs of whitespace into at most [limit] parts, the last one keeping the rest as it is.
pub fn split_whitespace(line: &str, limit: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        if parts.len() + 1 == limit {
            parts.push(rest);
            break;
        }
        match rest.find(char::is_whitespace) {
            Some(end) => {
                parts.push(&rest[..end]);
                rest = rest[end..].trim_start();
            }
            None => {
                parts.push(rest);
                break;
            }
        }
    }
    parts
}

/// The service view of `systemctl show`: the properties an agent needs, with systemd's "unset" as null.
pub fn unit(props: &BTreeMap<String, String>, redactor: &Redactor) -> Value {
    let number = |key: &str| props.get(key).and_then(|value| value.parse::<i64>().ok());
    // systemd shows an unset property as an empty value.
    let if_set = |key: &str| props.get(key).map(String::as_str).filter(|value| !value.is_empty());
    json!({
        "name": props.get("Id"),
        "description": props.get("Description").map(|description| redactor.redact(description)),
        "load": props.get("LoadState"),
        "active": props.get("ActiveState"),
        "sub": props.get("SubState"),
        "result": props.get("Result"),
        "enabled": if_set("UnitFileState"),
        "unit_file": if_set("FragmentPath"),
        "type": if_set("Type"),
        "restart": if_set("Restart"),
        "main_pid": number("MainPID").filter(|pid| *pid > 0),
        "exit_status": number("ExecMainStatus"),
        "restarts": number("NRestarts"),
        "memory_bytes": props
            .get("MemoryCurrent")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|bytes| *bytes != u64::MAX),
        "active_since": if_set("ActiveEnterTimestamp"),
        "state_changed": if_set("StateChangeTimestamp"),
    })
}

pub const PRIORITIES: [&str; 8] = ["emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"];

pub fn priority_number(name: &str) -> Option<usize> {
    PRIORITIES.iter().position(|priority| *priority == name)
}

/// Lookups in a JSON object that may be missing: `docker inspect` and the journal leave out what doesn't apply.
trait JsonObject<'a> {
    fn object(self, key: &str) -> Option<&'a Map<String, Value>>;
    fn string(self, key: &str) -> Option<&'a str>;
    /// The array, or an empty one.
    fn array(self, key: &str) -> &'a [Value];
    /// The value as it is, or null.
    fn value(self, key: &str) -> Value;
}

impl<'a> JsonObject<'a> for Option<&'a Map<String, Value>> {
    fn object(self, key: &str) -> Option<&'a Map<String, Value>> {
        self?.get(key)?.as_object()
    }

    fn string(self, key: &str) -> Option<&'a str> {
        self?.get(key)?.as_str()
    }

    fn array(self, key: &str) -> &'a [Value] {
        self.and_then(|object| object.get(key)?.as_array()).map_or(&[], Vec::as_slice)
    }

    fn value(self, key: &str) -> Value {
        self.and_then(|object| object.get(key)).cloned().unwrap_or(Value::Null)
    }
}

impl<'a> JsonObject<'a> for &'a Map<String, Value> {
    fn object(self, key: &str) -> Option<&'a Map<String, Value>> {
        Some(self).object(key)
    }

    fn string(self, key: &str) -> Option<&'a str> {
        Some(self).string(key)
    }

    fn array(self, key: &str) -> &'a [Value] {
        Some(self).array(key)
    }

    fn value(self, key: &str) -> Value {
        Some(self).value(key)
    }
}

/// `journalctl -o json`: one object per line. `MESSAGE` may be an array of bytes when it is not valid UTF-8.
pub fn journal(text: &str, redactor: &Redactor) -> Vec<Value> {
    text.lines().filter(|line| !line.trim().is_empty()).filter_map(|line| journal_entry(line, redactor)).collect()
}

/// One line of `journalctl -o json`; None when it is not a JSON object.
fn journal_entry(line: &str, redactor: &Redactor) -> Option<Value> {
    let record: Map<String, Value> = serde_json::from_str(line).ok()?;
    let micros = record.string("__REALTIME_TIMESTAMP").and_then(|micros| micros.parse::<i64>().ok());
    let priority = record
        .string("PRIORITY")
        .and_then(|number| number.parse::<usize>().ok())
        .and_then(|number| PRIORITIES.get(number));
    let mut entry = Map::new();
    entry.insert("time".into(), json!(micros.map(|micros| iso(micros / 1_000_000))));
    entry.insert("priority".into(), json!(priority));
    entry.insert("source".into(), json!(record.string("SYSLOG_IDENTIFIER").or(record.string("_COMM"))));
    entry.insert("pid".into(), json!(record.string("_PID").and_then(|pid| pid.parse::<i64>().ok())));
    if let Some(unit) = record.string("_SYSTEMD_UNIT") {
        entry.insert("unit".into(), json!(unit));
    }
    entry.insert("message".into(), json!(redactor.redact(&message(record.get("MESSAGE")))));
    Some(Value::Object(entry))
}

fn message(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(bytes)) => {
            let bytes: Vec<u8> = bytes.iter().filter_map(|byte| byte.as_u64().map(|byte| byte as u8)).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Some(other) => other.to_string(),
    }
}

/// `/proc/meminfo`, in bytes.
pub fn memory(text: &str) -> Value {
    let kilobytes: BTreeMap<&str, Option<i64>> = text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key, value.split_whitespace().next().and_then(|number| number.parse().ok())))
        .collect();
    let bytes = |key: &str| kilobytes.get(key).copied().flatten().map(|amount| amount * 1024);
    json!({
        "total_bytes": bytes("MemTotal"),
        "available_bytes": bytes("MemAvailable"),
        "swap_total_bytes": bytes("SwapTotal"),
        "swap_free_bytes": bytes("SwapFree"),
    })
}

/// `/etc/os-release`'s `PRETTY_NAME`.
pub fn os_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_string())
}

/// The length of Docker's short form of an ID.
const SHORT_ID_LENGTH: usize = 12;
/// `container` shows the last runs of a health check, and the start of each one's output.
const HEALTH_RUNS_SHOWN: usize = 5;
const HEALTH_OUTPUT_CHARS: usize = 500;

/// The list view of one `docker inspect` object.
pub fn container_summary(container: &Map<String, Value>) -> Value {
    let state = container.object("State");
    let config = container.object("Config");
    let labels = config.object("Labels");
    json!({
        "name": container_name(container),
        "image": config.string("Image"),
        "state": state.string("Status"),
        "health": state.object("Health").string("Status"),
        "restarts": container.get("RestartCount").and_then(Value::as_i64),
        "started": state.string("StartedAt"),
        "compose_project": labels.string("com.docker.compose.project"),
        "compose_service": labels.string("com.docker.compose.service"),
    })
}

/// The detail view of one `docker inspect` object: never an environment value, only names (spec §5).
pub fn container_detail(container: &Map<String, Value>, digests: &[String], redactor: &Redactor) -> Value {
    let config = container.object("Config");
    let network_settings = container.object("NetworkSettings");
    let command: Vec<&str> =
        container.string("Path").into_iter().chain(container.array("Args").iter().filter_map(Value::as_str)).collect();
    json!({
        "id": container.string("Id").map(|id| id.chars().take(SHORT_ID_LENGTH).collect::<String>()),
        "name": container_name(container),
        "image": config.string("Image"),
        "image_id": container.string("Image"),
        "image_digests": digests,
        "created": container.string("Created"),
        "state": container_state(container.object("State"), redactor),
        "restart_count": container.value("RestartCount"),
        "restart_policy": container.object("HostConfig").object("RestartPolicy").string("Name"),
        "command": redactor.redact(&command.join(" ")),
        "mounts": container_mounts(container),
        "ports": network_settings.value("Ports"),
        "networks": container_networks(network_settings),
        "labels": container_labels(config, redactor),
        "env": env_names(config),
    })
}

/// Docker keeps a container's name with a leading `/`.
fn container_name(container: &Map<String, Value>) -> Option<&str> {
    container.string("Name").map(|name| name.trim_start_matches('/'))
}

fn container_state(state: Option<&Map<String, Value>>, redactor: &Redactor) -> Value {
    let mut view = json!({
        "status": state.string("Status"),
        "running": state.value("Running"),
        "started_at": state.string("StartedAt"),
        "finished_at": state.string("FinishedAt"),
        "exit_code": state.value("ExitCode"),
        "oom_killed": state.value("OOMKilled"),
        "error": state.string("Error").filter(|error| !error.is_empty()).map(|error| redactor.redact(error)),
    });
    if let Some(health) = state.object("Health") {
        view["health"] = health_check(health, redactor);
    }
    view
}

fn health_check(health: &Map<String, Value>, redactor: &Redactor) -> Value {
    let runs = health.array("Log");
    let last_runs: Vec<Value> = runs[runs.len().saturating_sub(HEALTH_RUNS_SHOWN)..]
        .iter()
        .filter_map(Value::as_object)
        .map(|run| {
            let output: String =
                redactor.redact(run.string("Output").unwrap_or("").trim()).chars().take(HEALTH_OUTPUT_CHARS).collect();
            json!({"start": run.string("Start"), "exit_code": run.value("ExitCode"), "output": output})
        })
        .collect();
    json!({
        "status": health.string("Status"),
        "failing_streak": health.value("FailingStreak"),
        "log": last_runs,
    })
}

fn container_mounts(container: &Map<String, Value>) -> Vec<Value> {
    container
        .array("Mounts")
        .iter()
        .filter_map(Value::as_object)
        .map(|mount| {
            json!({
                "type": mount.string("Type"),
                "source": mount.string("Source"),
                "destination": mount.string("Destination"),
                "rw": mount.value("RW"),
            })
        })
        .collect()
}

/// Each network the container is on, with its address there.
fn container_networks(network_settings: Option<&Map<String, Value>>) -> Map<String, Value> {
    let Some(networks) = network_settings.object("Networks") else {
        return Map::new();
    };
    networks
        .iter()
        .map(|(name, network)| {
            let ip = network.get("IPAddress").and_then(Value::as_str).filter(|ip| !ip.is_empty());
            (name.clone(), json!({"ip": ip}))
        })
        .collect()
}

fn container_labels(config: Option<&Map<String, Value>>, redactor: &Redactor) -> Map<String, Value> {
    let Some(labels) = config.object("Labels") else {
        return Map::new();
    };
    labels.iter().map(|(key, value)| (key.clone(), redacted_label(key, value, redactor))).collect()
}

/// Redacted with its key: `db.password=hunter2` is what the patterns recognise; `hunter2` alone isn't.
fn redacted_label(key: &str, value: &Value, redactor: &Redactor) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    let assignment = format!("{key}=");
    let redacted = redactor.redact(&format!("{assignment}{text}"));
    json!(redacted.strip_prefix(&assignment).unwrap_or(&redacted))
}

fn env_names(config: Option<&Map<String, Value>>) -> Vec<&str> {
    config
        .array("Env")
        .iter()
        .filter_map(Value::as_str)
        .map(|variable| variable.split_once('=').map_or(variable, |(name, _)| name))
        .collect()
}

/// `docker logs --timestamps` lines: `2026-09-26T08:00:00.123456789Z message`, with the stream they came on. The
/// timestamp is what merges stdout and stderr back in order.
pub fn docker_log_lines(text: &str, stream: &'static str) -> Vec<(String, &'static str, String)> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(' ') {
            Some((time, rest)) => (time.to_string(), stream, rest.to_string()),
            None => (line.to_string(), stream, String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_of_list_units() {
        let rows =
            units("ssh.service    loaded active   running OpenBSD Secure  Shell server\n\nbad\n", &Redactor::default());
        assert_eq!(rows[0]["unit"], "ssh.service");
        assert_eq!(rows[0]["active"], "active");
        assert_eq!(rows[0]["description"], "OpenBSD Secure  Shell server");
        assert_eq!(rows.as_array().unwrap().len(), 1);
    }

    #[test]
    fn unit_show_with_unset_values() {
        let props = key_values(
            "Id=nginx.service\nMainPID=0\nMemoryCurrent=18446744073709551615\nUnitFileState=\nNRestarts=3\nActiveState=active",
        );
        let service = unit(&props, &Redactor::default());
        assert_eq!(service["name"], "nginx.service");
        assert_eq!(service["main_pid"], Value::Null);
        assert_eq!(service["memory_bytes"], Value::Null);
        assert_eq!(service["enabled"], Value::Null);
        assert_eq!(service["restarts"], 3);
    }

    #[test]
    fn journal_entries() {
        let text = r#"{"__REALTIME_TIMESTAMP":"1790409600000000","PRIORITY":"3","SYSLOG_IDENTIFIER":"sshd","_PID":"12","MESSAGE":"login failed password=hunter2"}
{"__REALTIME_TIMESTAMP":"1790409601000000","_COMM":"bin","MESSAGE":[104,105]}
not json"#;
        let entries = journal(text, &Redactor::default());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["time"], "2026-09-26T08:00:00Z");
        assert_eq!(entries[0]["priority"], "err");
        assert_eq!(entries[0]["message"], "login failed password=[redacted]");
        assert_eq!(entries[1]["message"], "hi");
        assert_eq!(entries[1]["source"], "bin");
    }

    #[test]
    fn container_detail_hides_environment_values() {
        let inspect: Map<String, Value> = serde_json::from_str(
            r#"{"Id":"0123456789abcdef","Name":"/immich","Image":"sha256:aa","Path":"start.sh","Args":["--token=abc"],
              "State":{"Status":"running","Running":true,"ExitCode":0,
                "Health":{"Status":"healthy","FailingStreak":0,"Log":[{"Start":"t","ExitCode":0,"Output":"ok\n"}]}},
              "RestartCount":1,"HostConfig":{"RestartPolicy":{"Name":"unless-stopped"}},
              "Config":{"Image":"ghcr.io/immich:v1","Env":["DB_PASSWORD=secret","TZ=UTC"],
                "Labels":{"com.docker.compose.project":"photos","auth":"password=hunter2","db.password":"s3cr3t"}},
              "Mounts":[{"Type":"bind","Source":"/srv","Destination":"/data","RW":true}],
              "NetworkSettings":{"Ports":{"80/tcp":null},"Networks":{"photos_default":{"IPAddress":"172.18.0.2"}}}}"#,
        )
        .unwrap();
        let detail = container_detail(&inspect, &["ghcr.io/immich@sha256:bb".into()], &Redactor::default());
        assert_eq!(detail["env"], json!(["DB_PASSWORD", "TZ"]));
        assert_eq!(detail["command"], "start.sh --token=[redacted]");
        assert_eq!(detail["state"]["health"]["status"], "healthy");
        assert_eq!(detail["id"], "0123456789ab");
        let text = detail.to_string();
        assert!(!text.contains("secret\""), "{text}");
        assert!(!text.contains("hunter2"), "a label's value is redacted like everything else");
        assert!(!text.contains("s3cr3t"), "and the key of a label says when its value is a secret");
        let summary = container_summary(&inspect);
        assert_eq!(summary["compose_project"], "photos");
        assert_eq!(summary["name"], "immich");
    }

    #[test]
    fn memory_and_os() {
        let mem = memory("MemTotal:       16000 kB\nMemAvailable:    8000 kB\nSwapTotal: 0 kB\n");
        assert_eq!(mem["total_bytes"], 16_384_000);
        assert_eq!(mem["swap_free_bytes"], Value::Null);
        assert_eq!(
            os_name("NAME=x\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n").as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
    }
}
