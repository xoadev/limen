//! What the gate reads from the programs it runs, turned into the JSON it answers.

use crate::redactor::Redactor;
use crate::time::iso;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// `systemctl show` output: one `Key=Value` per line.
pub fn key_values(text: &str) -> BTreeMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// `systemctl list-units --no-legend --plain`: UNIT LOAD ACTIVE SUB DESCRIPTION. A description is whatever the unit
/// file says, so it is redacted like any other text.
pub fn units(text: &str, redactor: &Redactor) -> Value {
    let rows: Vec<Value> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
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
    let mut out = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        if out.len() + 1 == limit {
            out.push(rest);
            break;
        }
        match rest.find(char::is_whitespace) {
            Some(i) => {
                out.push(&rest[..i]);
                rest = rest[i..].trim_start();
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

fn non_empty(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str).filter(|s| !s.is_empty())
}

/// The service view of `systemctl show`: the properties an agent needs, with systemd's "unset" as null.
pub fn unit(props: &BTreeMap<String, String>, redactor: &Redactor) -> Value {
    let num = |k: &str| props.get(k).and_then(|v| v.parse::<i64>().ok());
    json!({
        "name": props.get("Id"),
        "description": props.get("Description").map(|d| redactor.redact(d)),
        "load": props.get("LoadState"),
        "active": props.get("ActiveState"),
        "sub": props.get("SubState"),
        "result": props.get("Result"),
        "enabled": non_empty(props.get("UnitFileState")),
        "unit_file": non_empty(props.get("FragmentPath")),
        "type": non_empty(props.get("Type")),
        "restart": non_empty(props.get("Restart")),
        "main_pid": num("MainPID").filter(|p| *p > 0),
        "exit_status": num("ExecMainStatus"),
        "restarts": num("NRestarts"),
        "memory_bytes": props.get("MemoryCurrent").and_then(|v| v.parse::<u64>().ok()).filter(|m| *m != u64::MAX),
        "active_since": non_empty(props.get("ActiveEnterTimestamp")),
        "state_changed": non_empty(props.get("StateChangeTimestamp")),
    })
}

pub const PRIORITIES: [&str; 8] = ["emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"];

pub fn priority_number(name: &str) -> Option<usize> {
    PRIORITIES.iter().position(|p| *p == name)
}

fn str_of<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str)
}

/// `journalctl -o json`: one object per line. `MESSAGE` may be an array of bytes when it is not valid UTF-8.
pub fn journal(text: &str, redactor: &Redactor) -> Vec<Value> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let o: Map<String, Value> = serde_json::from_str(line).ok()?;
            let micros = str_of(&o, "__REALTIME_TIMESTAMP").and_then(|m| m.parse::<i64>().ok());
            let mut entry = Map::new();
            entry.insert("time".into(), json!(micros.map(|m| iso(m / 1_000_000))));
            entry.insert(
                "priority".into(),
                json!(str_of(&o, "PRIORITY").and_then(|p| p.parse::<usize>().ok()).and_then(|p| PRIORITIES.get(p))),
            );
            entry.insert("source".into(), json!(str_of(&o, "SYSLOG_IDENTIFIER").or(str_of(&o, "_COMM"))));
            entry.insert("pid".into(), json!(str_of(&o, "_PID").and_then(|p| p.parse::<i64>().ok())));
            if let Some(unit) = str_of(&o, "_SYSTEMD_UNIT") {
                entry.insert("unit".into(), json!(unit));
            }
            entry.insert("message".into(), json!(redactor.redact(&message(o.get("MESSAGE")))));
            Some(Value::Object(entry))
        })
        .collect()
}

fn message(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(bytes)) => {
            let bytes: Vec<u8> = bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Some(other) => other.to_string(),
    }
}

/// `/proc/meminfo`, in bytes.
pub fn memory(text: &str) -> Value {
    let kb: BTreeMap<&str, Option<i64>> = text
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k, v.split_whitespace().next().and_then(|n| n.parse().ok())))
        .collect();
    let bytes = |k: &str| kb.get(k).copied().flatten().map(|n| n * 1024);
    json!({
        "total_bytes": bytes("MemTotal"),
        "available_bytes": bytes("MemAvailable"),
        "swap_total_bytes": bytes("SwapTotal"),
        "swap_free_bytes": bytes("SwapFree"),
    })
}

/// `/etc/os-release`'s `PRETTY_NAME`.
pub fn os_name(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim().trim_matches('"').to_string())
}

fn obj<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    o.get(key).and_then(Value::as_object)
}

fn arr<'a>(o: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    o.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn field(o: Option<&Map<String, Value>>, key: &str) -> Value {
    o.and_then(|o| o.get(key)).cloned().unwrap_or(Value::Null)
}

/// The list view of one `docker inspect` object.
pub fn container_summary(o: &Map<String, Value>) -> Value {
    let state = obj(o, "State");
    let config = obj(o, "Config");
    let labels = config.and_then(|c| obj(c, "Labels"));
    json!({
        "name": str_of(o, "Name").map(|n| n.trim_start_matches('/')),
        "image": config.and_then(|c| str_of(c, "Image")),
        "state": state.and_then(|s| str_of(s, "Status")),
        "health": state.and_then(|s| obj(s, "Health")).and_then(|h| str_of(h, "Status")),
        "restarts": o.get("RestartCount").and_then(Value::as_i64),
        "started": state.and_then(|s| str_of(s, "StartedAt")),
        "compose_project": labels.and_then(|l| str_of(l, "com.docker.compose.project")),
        "compose_service": labels.and_then(|l| str_of(l, "com.docker.compose.service")),
    })
}

/// The detail view of one `docker inspect` object: never an environment value, only names (spec §5).
pub fn container_detail(o: &Map<String, Value>, digests: &[String], redactor: &Redactor) -> Value {
    let state = obj(o, "State");
    let config = obj(o, "Config");
    let mut state_out = json!({
        "status": state.and_then(|s| str_of(s, "Status")),
        "running": field(state, "Running"),
        "started_at": state.and_then(|s| str_of(s, "StartedAt")),
        "finished_at": state.and_then(|s| str_of(s, "FinishedAt")),
        "exit_code": field(state, "ExitCode"),
        "oom_killed": field(state, "OOMKilled"),
        "error": state.and_then(|s| str_of(s, "Error")).filter(|e| !e.is_empty()).map(|e| redactor.redact(e)),
    });
    if let Some(health) = state.and_then(|s| obj(s, "Health")) {
        let log = arr(health, "Log");
        let log: Vec<Value> = log[log.len().saturating_sub(5)..]
            .iter()
            .filter_map(Value::as_object)
            .map(|e| {
                let output: String =
                    redactor.redact(str_of(e, "Output").unwrap_or("").trim()).chars().take(500).collect();
                json!({"start": str_of(e, "Start"), "exit_code": field(Some(e), "ExitCode"), "output": output})
            })
            .collect();
        state_out["health"] = json!({
            "status": str_of(health, "Status"),
            "failing_streak": field(Some(health), "FailingStreak"),
            "log": log,
        });
    }
    let command: Vec<&str> =
        str_of(o, "Path").into_iter().chain(arr(o, "Args").iter().filter_map(Value::as_str)).collect();
    let mounts: Vec<Value> = arr(o, "Mounts")
        .iter()
        .filter_map(Value::as_object)
        .map(|m| {
            json!({
                "type": str_of(m, "Type"),
                "source": str_of(m, "Source"),
                "destination": str_of(m, "Destination"),
                "rw": field(Some(m), "RW"),
            })
        })
        .collect();
    let networks: Map<String, Value> = obj(o, "NetworkSettings")
        .and_then(|n| obj(n, "Networks"))
        .map(|nets| {
            nets.iter()
                .map(|(name, net)| {
                    let ip = net.get("IPAddress").and_then(Value::as_str).filter(|ip| !ip.is_empty());
                    (name.clone(), json!({"ip": ip}))
                })
                .collect()
        })
        .unwrap_or_default();
    // With its key: `db.password=hunter2` is what the patterns recognise; `hunter2` alone isn't.
    let labels: Map<String, Value> = config
        .and_then(|c| obj(c, "Labels"))
        .map(|labels| {
            labels
                .iter()
                .map(|(k, v)| {
                    let value = match v.as_str() {
                        Some(s) => {
                            let redacted = redactor.redact(&format!("{k}={s}"));
                            json!(redacted.strip_prefix(&format!("{k}=")).unwrap_or(&redacted))
                        }
                        None => v.clone(),
                    };
                    (k.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    let env: Vec<&str> = config
        .map(|c| arr(c, "Env"))
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .map(|e| e.split_once('=').map_or(e, |(name, _)| name))
        .collect();
    json!({
        "id": str_of(o, "Id").map(|id| id.chars().take(12).collect::<String>()),
        "name": str_of(o, "Name").map(|n| n.trim_start_matches('/')),
        "image": config.and_then(|c| str_of(c, "Image")),
        "image_id": str_of(o, "Image"),
        "image_digests": digests,
        "created": str_of(o, "Created"),
        "state": state_out,
        "restart_count": field(Some(o), "RestartCount"),
        "restart_policy": obj(o, "HostConfig").and_then(|h| obj(h, "RestartPolicy")).and_then(|r| str_of(r, "Name")),
        "command": redactor.redact(&command.join(" ")),
        "mounts": mounts,
        "ports": obj(o, "NetworkSettings").map(|n| field(Some(n), "Ports")).unwrap_or(Value::Null),
        "networks": networks,
        "labels": labels,
        "env": env,
    })
}

/// `docker logs --timestamps` lines: `2026-09-26T08:00:00.123456789Z message`, with the stream they came on. The
/// timestamp is what merges stdout and stderr back in order.
pub fn docker_log_lines(text: &str, stream: &'static str) -> Vec<(String, &'static str, String)> {
    text.lines()
        .filter(|l| !l.is_empty())
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
        let u = unit(&props, &Redactor::default());
        assert_eq!(u["name"], "nginx.service");
        assert_eq!(u["main_pid"], Value::Null);
        assert_eq!(u["memory_bytes"], Value::Null);
        assert_eq!(u["enabled"], Value::Null);
        assert_eq!(u["restarts"], 3);
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
