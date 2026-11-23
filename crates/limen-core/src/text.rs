//! Answers as a person reads them (spec §5, §10): a script's output and a file's lines as text, not JSON.

use serde_json::Value;

/// What a cut answer ends with.
pub const TRUNCATED: &str = "[truncated: a limit cut this answer; narrow the request]";

/// The text of [request]'s answer [data], for the requests that read as text: `run` and `read_file`.
pub fn answer_text(request: &str, data: &Value) -> Option<String> {
    match request {
        "run" => Some(format!("{}{}", output_text(data), exit_line(data))),
        "read_file" => Some(file_text(data)),
        _ => None,
    }
}

/// What a script printed: its stdout, then its stderr after a `[stderr]` line; each ends in a newline.
pub fn output_text(data: &Value) -> String {
    let mut text = String::new();
    for (stream, heading) in [("stdout", ""), ("stderr", "[stderr]\n")] {
        let output = string(data, stream);
        if !output.is_empty() {
            text.push_str(heading);
            text.push_str(output);
            if !output.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    text
}

/// A script's exit code, and the signal that ended it if one did: `[exit 0]`, `[exit -1, signal 15]`.
pub fn exit_line(data: &Value) -> String {
    let exit = data.get("exit").and_then(Value::as_i64).unwrap_or(-1);
    match data.get("signal").and_then(Value::as_i64) {
        Some(signal) => format!("[exit {exit}, signal {signal}]"),
        None => format!("[exit {exit}]"),
    }
}

/// A file's lines, then the path and which lines they are.
fn file_text(data: &Value) -> String {
    let path = string(data, "path");
    if data.get("binary").and_then(Value::as_bool) == Some(true) {
        let size = data.get("size_bytes").and_then(Value::as_u64).unwrap_or_default();
        return format!("[{path}: a binary file, {size} bytes]");
    }
    let Some(content) = data.get("content").and_then(Value::as_str) else {
        let lines: Vec<&str> = data["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        return match lines.len() {
            0 => format!("[{path}: no line]"),
            count => format!("{}\n[{path}: its last {count} lines]", lines.join("\n")),
        };
    };
    let (from, to) = (number(data, "from"), number(data, "to"));
    let eof = if data.get("eof").and_then(Value::as_bool) == Some(true) { ", end of file" } else { "" };
    if to < from {
        return format!("[{path}: nothing from line {from}{eof}]");
    }
    format!("{content}\n[{path}: lines {from}–{to}{eof}]")
}

fn string<'a>(data: &'a Value, key: &str) -> &'a str {
    data.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(data: &Value, key: &str) -> i64 {
    data.get(key).and_then(Value::as_i64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_script_reads_as_its_output_and_its_exit() {
        let ran = json!({"script": "disk", "exit": 1, "stdout": "full\n", "stderr": "warning"});
        assert_eq!(answer_text("run", &ran).as_deref(), Some("full\n[stderr]\nwarning\n[exit 1]"));
        let killed = json!({"exit": -1, "signal": 15, "stdout": "", "stderr": ""});
        assert_eq!(answer_text("run", &killed).as_deref(), Some("[exit -1, signal 15]"));
    }

    #[test]
    fn a_file_reads_as_its_lines_and_where_they_are() {
        let range = json!({"path": "/etc/hosts", "from": 1, "to": 2, "eof": true, "content": "a\nb"});
        assert_eq!(answer_text("read_file", &range).as_deref(), Some("a\nb\n[/etc/hosts: lines 1–2, end of file]"));
        let past_the_end = json!({"path": "/etc/hosts", "from": 9, "to": 8, "eof": true, "content": ""});
        assert_eq!(file_text(&past_the_end), "[/etc/hosts: nothing from line 9, end of file]");
        let end = json!({"path": "/var/log/x", "lines": ["y", "z"]});
        assert_eq!(file_text(&end), "y\nz\n[/var/log/x: its last 2 lines]");
        assert_eq!(file_text(&json!({"path": "/b", "binary": true, "size_bytes": 3})), "[/b: a binary file, 3 bytes]");
        assert_eq!(answer_text("list_dir", &json!({})), None);
    }
}
