//! `read_file` and `list_dir` (spec §5, §7.1): the files the node allows, walked as the kernel walks them and opened
//! once, so what is checked is what is read.

use super::{Answer, Node, internal};
use crate::os::fs;
use limen_core::params::ArgsExt;
use limen_core::protocol::{ErrorCode, LimenError, Result, bad_request, error};
use limen_core::time::iso;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

type Args = Map<String, Value>;

const PRIVATE_KEY: &[u8] = b"PRIVATE KEY-----";
/// What `read_file` with `grep` or `tail` reads at most, from the end.
const MAX_SCAN_BYTES: u64 = 16 << 20;
/// What `read_file` with `grep` or `tail` reads per line it looks at, up to [MAX_SCAN_BYTES].
const BYTES_PER_LINE: u64 = 1024;
/// How far into a file `read_file` goes to reach its first line: further on, `tail` reads the end.
const MAX_SKIP_BYTES: u64 = 64 << 20;
const KEY_FILE_MAX_BYTES: u64 = 1 << 20;
/// Links followed in one path, as the kernel's own limit.
const MAX_LINKS: usize = 40;
const MAX_LISTED_ENTRIES: usize = 1000;
const DEFAULT_LINES: usize = 500;

pub fn read_file(node: &Node, args: &Args) -> Result<Answer> {
    let opened = allowed_file(node, args.string("path").unwrap_or_default())?;
    let grep = args.string("grep");
    let tail = args.small_integer("tail")?.map(|tail| tail.max(1) as usize);
    let ranged = args.contains_key("from") || args.contains_key("lines");
    match (ranged, grep.is_some() || tail.is_some()) {
        (true, true) => Err(bad_request("read_file takes from and lines, or grep and tail: not both")),
        (_, true) => file_end(node, &opened, grep, tail),
        _ => file_range(node, &opened, args),
    }
}

/// `from` and `lines`: a range of the file.
fn file_range(node: &Node, opened: &fs::Opened, args: &Args) -> Result<Answer> {
    let (path, size) = (&opened.info.path, opened.info.size);
    let from = args.small_integer("from")?.unwrap_or(1).max(1) as usize;
    let count = args.small_integer("lines")?.map_or(DEFAULT_LINES, |lines| lines.max(1) as usize);
    let slice = fs::read_lines(&opened.file, from, count, node.config.max_file_bytes, MAX_SKIP_BYTES)
        .map_err(|reason| bad_request(format!("{path}: {reason}")))?;
    if slice.binary {
        return Ok(Answer::of(json!({"path": path, "size_bytes": size, "binary": true})));
    }
    let cut_by_bytes = !slice.eof && slice.lines.len() < count;
    Ok(Answer::cut(
        json!({
            "path": path,
            "size_bytes": size,
            "from": from,
            "to": from + slice.lines.len() - 1,
            "eof": slice.eof,
            "content": node.redactor.redact(&slice.lines.join("\n")),
        }),
        cut_by_bytes,
    ))
}

/// `grep` and `tail`: the last lines of the file, or the last that match, within its last `limits.scan_lines`.
fn file_end(node: &Node, opened: &fs::Opened, grep: Option<&str>, tail: Option<usize>) -> Result<Answer> {
    let path = &opened.info.path;
    let window = if grep.is_some() { node.config.scan_lines } else { tail.unwrap_or(node.config.max_lines) };
    let window = window.min(node.config.scan_lines);
    let end = fs::tail(&opened.file, window, (window as u64 * BYTES_PER_LINE).min(MAX_SCAN_BYTES))
        .map_err(|reason| internal(format!("cannot read {path}: {reason}")))?;
    if end.binary {
        return Ok(Answer::of(json!({"path": path, "size_bytes": opened.info.size, "binary": true})));
    }
    // Redacted before it is cut into lines: a private key the window holds whole spans several of them.
    let (lines, cut) = node.filter(&node.redactor.redact(&end.lines.join("\n")), grep, tail);
    Ok(Answer::cut(json!({"path": path, "size_bytes": opened.info.size, "lines": lines}), cut))
}

pub fn list_dir(node: &Node, args: &Args) -> Result<Answer> {
    let requested = args.string("path").unwrap_or_default();
    let dir = resolve(node, requested, &|path| visible(node, path))?;
    let opened = fs::open_dir_exact(&dir).map_err(|failure| match failure {
        fs::OpenError::Missing => does_not_exist(requested),
        fs::OpenError::NotDirectory => bad_request(format!("{requested} is not a directory")),
        fs::OpenError::Other(reason) => internal(reason),
    })?;
    // Read once for the whole listing, not per entry.
    let (users, groups) = (IdNames::users(), IdNames::groups());
    let mut entries: Vec<Value> = opened
        .entries()
        .map_err(internal)?
        .iter()
        .filter_map(|(name, entry)| listed_entry(node, &users, &groups, name, entry))
        .take(MAX_LISTED_ENTRIES + 1)
        .collect();
    let truncated = entries.len() > MAX_LISTED_ENTRIES;
    entries.truncate(MAX_LISTED_ENTRIES);
    Ok(Answer::cut(json!({"path": dir, "entries": entries}), truncated))
}

/// [entry] as `list_dir` shows it; none when the client may not see it. A link is shown where it leads, if that is
/// somewhere the client may see.
fn listed_entry(node: &Node, users: &IdNames, groups: &IdNames, name: &str, entry: &fs::FileInfo) -> Option<Value> {
    let (target, kind) = match entry.kind {
        fs::FileType::Link => match walk(node, &entry.path) {
            Walked::Found(target) => {
                let kind = fs::stat(&target).map(|info| info.kind);
                (target, kind)
            }
            Walked::Missing(_) | Walked::Hidden => return None,
        },
        kind => (entry.path.clone(), Some(kind)),
    };
    let shown =
        node.policy.allowed(&target) || (kind == Some(fs::FileType::Directory) && node.policy.leads_to(&target));
    if !shown {
        return None;
    }
    let mut listed = json!({
        "name": name,
        "type": entry.kind.wire(),
        "size_bytes": entry.size,
        "mode": format!("{:04o}", entry.mode & 0o7777),
        "owner": users.name(entry.uid),
        "group": groups.name(entry.gid),
        "modified": iso(entry.modified),
    });
    if entry.kind == fs::FileType::Link {
        listed["target"] = json!(target);
    }
    Some(listed)
}

/// User or group names by id, read once for a whole listing.
struct IdNames(BTreeMap<u32, String>);

impl IdNames {
    fn users() -> Self {
        IdNames(fs::accounts().into_iter().map(|account| (account.uid, account.name)).collect())
    }

    fn groups() -> Self {
        IdNames(fs::groups())
    }

    /// The name of [id], or the number itself when it has none.
    fn name(&self, id: u32) -> String {
        self.0.get(&id).cloned().unwrap_or_else(|| id.to_string())
    }
}

/// [requested] opened for reading, once: what is checked —the policy, the type, hard links, keys— is checked on what
/// is then read.
pub fn allowed_file(node: &Node, requested: &str) -> Result<fs::Opened> {
    let resolved = resolve(node, requested, &|path| node.policy.allowed(path))?;
    let opened = fs::open_exact(&resolved).map_err(|failure| match failure {
        fs::OpenError::Missing => does_not_exist(requested),
        fs::OpenError::NotDirectory | fs::OpenError::Other(_) => internal(format!("cannot open {requested}")),
    })?;
    if opened.info.kind != fs::FileType::File {
        return Err(bad_request(format!("{requested} is not a regular file")));
    }
    // Another name for the same file could be anywhere, a denied place included: a hard link into an allowed
    // directory would carry the policy with it.
    if opened.info.links > 1 {
        return Err(error(ErrorCode::Denied, format!("{requested} is not readable: it has other hard links")));
    }
    // A window of lines can fall between a key's markers, where redaction can't see it. Keys live in small files, so
    // those are searched whole; in a large one, a log, the window is redacted as a whole instead.
    if opened.info.size <= KEY_FILE_MAX_BYTES && fs::contains(&opened.file, PRIVATE_KEY, KEY_FILE_MAX_BYTES) {
        return Err(error(ErrorCode::Denied, format!("{requested} holds a private key")));
    }
    Ok(opened)
}

/// Where a path leads, as far as the client may know.
enum Walked {
    Found(String),
    /// It doesn't exist; this is where it would be.
    Missing(String),
    /// On the way it passes somewhere the policy hides.
    Hidden,
}

/// [path] walked one component at a time, as the kernel does: links followed, `..` taken from where the walk is,
/// not from the text. Every step must be allowed or lead to something allowed, so nothing is learnt of a place the
/// policy hides, not even whether it exists. Past a missing component the walk goes on as text, judged the same way.
fn walk(node: &Node, path: &str) -> Walked {
    let mut pending = components_last_first(path);
    let mut current = String::from("/");
    let mut missing = false;
    let mut links_followed = 0;
    while let Some(component) = pending.pop() {
        match component.as_str() {
            "." => continue,
            ".." => {
                current = fs::parent(&current).into();
                continue;
            }
            _ => {}
        }
        let next = if current == "/" { format!("/{component}") } else { format!("{current}/{component}") };
        if !visible(node, &next) {
            return Walked::Hidden;
        }
        if !missing {
            match fs::lstat(&next) {
                None => missing = true,
                Some(info) if info.kind == fs::FileType::Link => {
                    links_followed += 1;
                    let Some(target) = fs::read_link(&next).filter(|_| links_followed <= MAX_LINKS) else {
                        return Walked::Hidden;
                    };
                    if target.starts_with('/') {
                        current = "/".into();
                    }
                    pending.extend(components_last_first(&target));
                    continue;
                }
                Some(_) => {}
            }
        }
        current = next;
    }
    if missing { Walked::Missing(current) } else { Walked::Found(current) }
}

/// The components of [path], last first, for the walk to pop.
fn components_last_first(path: &str) -> Vec<String> {
    path.split('/').rev().filter(|component| !component.is_empty()).map(String::from).collect()
}

/// Whether the client may see [path]: it is allowed, or on the way to something allowed.
fn visible(node: &Node, path: &str) -> bool {
    node.policy.allowed(path) || node.policy.leads_to(path)
}

/// [requested] walked (see [walk]) and judged by [permits]. A refusal names the path as it was asked for and says no
/// more: the reason would tell where it leads.
fn resolve(node: &Node, requested: &str, permits: &dyn Fn(&str) -> bool) -> Result<String> {
    if !requested.starts_with('/') {
        return Err(bad_request(format!("{requested} is not an absolute path")));
    }
    match walk(node, requested) {
        Walked::Found(path) if permits(&path) => Ok(path),
        Walked::Missing(path) if permits(&path) => Err(does_not_exist(requested)),
        _ => Err(error(ErrorCode::Denied, format!("{requested} is not readable"))),
    }
}

fn does_not_exist(requested: &str) -> LimenError {
    error(ErrorCode::NotFound, format!("{requested} does not exist"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::last_matching;
    use limen_core::config::node::NodeConfig;
    use std::os::unix::fs::symlink;

    /// A tree to read, removed at the end: allowed files, a denied directory, links out of the allowlist.
    struct Tree {
        dir: String,
        node: Node,
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn tree() -> Tree {
        let dir = format!("{}/limen-files-{}", std::env::temp_dir().display(), crate::hub::dir::random(8).unwrap());
        for subdir in ["etc/private", "outside"] {
            std::fs::create_dir_all(format!("{dir}/{subdir}")).unwrap();
        }
        let write = |file: &str, text: &str| std::fs::write(format!("{dir}/{file}"), text).unwrap();
        write("etc/app.conf", "name = app\npassword = \"two words\"\n");
        write("etc/private/real", "x\n");
        write("outside/real", "x\n");
        write(
            "etc/deploy.pem",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
        );
        symlink(format!("{dir}/outside/real"), format!("{dir}/etc/link")).unwrap();
        symlink(format!("{dir}/etc/private"), format!("{dir}/etc/privlink")).unwrap();
        let config = NodeConfig {
            allow: vec![format!("{dir}/etc/**")],
            deny: vec![format!("{dir}/etc/private/**")],
            audit: format!("{dir}/audit.jsonl"),
            ..Default::default()
        };
        Tree { node: Node::new(config), dir }
    }

    fn path_args(path: &str) -> Args {
        args_of(json!({"path": path}))
    }

    fn args_of(value: Value) -> Args {
        match value {
            Value::Object(args) => args,
            other => panic!("{other} is not an object"),
        }
    }

    fn refused(result: Result<Answer>) -> ErrorCode {
        result.err().expect("refused").code
    }

    #[test]
    fn an_allowed_file_is_read_and_redacted() {
        let tree = tree();
        let answer = read_file(&tree.node, &path_args(&format!("{}/etc/app.conf", tree.dir))).unwrap();
        assert_eq!(answer.data["content"], "name = app\npassword = \"[redacted]\"");
        assert_eq!(answer.data["to"], 2);
    }

    #[test]
    fn a_denied_path_says_nothing_of_its_existence() {
        let tree = tree();
        for denied in [
            "private/real",
            "private/missing",
            "../outside/real",
            "../outside/missing",
            "link",
            "privlink/real",
            "privlink/missing",
        ] {
            let path = format!("{}/etc/{denied}", tree.dir);
            assert_eq!(refused(read_file(&tree.node, &path_args(&path))), ErrorCode::Denied, "read_file {denied}");
            assert_eq!(refused(list_dir(&tree.node, &path_args(&path))), ErrorCode::Denied, "list_dir {denied}");
        }
        let missing = format!("{}/etc/missing", tree.dir);
        assert_eq!(refused(read_file(&tree.node, &path_args(&missing))), ErrorCode::NotFound);
        let file = format!("{}/etc/app.conf", tree.dir);
        assert_eq!(refused(list_dir(&tree.node, &path_args(&file))), ErrorCode::BadRequest);
    }

    #[test]
    fn dots_are_walked_where_links_lead_not_as_text() {
        // As text, etc/up/../x is etc/x, allowed; the kernel reads outside/x, which is not. Whether it exists must not
        // show either.
        let tree = tree();
        std::fs::create_dir_all(format!("{}/outside/sub", tree.dir)).unwrap();
        symlink(format!("{}/outside/sub", tree.dir), format!("{}/etc/up", tree.dir)).unwrap();
        for relative in ["up/../real", "up/../missing", "privlink/../../outside/real"] {
            let path = format!("{}/etc/{relative}", tree.dir);
            let refusal = read_file(&tree.node, &path_args(&path)).err().expect("refused");
            assert_eq!((refusal.code, refusal.message), (ErrorCode::Denied, format!("{path} is not readable")));
        }
        // Where it does lead somewhere allowed, it is read.
        let back = format!("{}/etc/private/../app.conf", tree.dir);
        assert!(read_file(&tree.node, &path_args(&back)).is_err(), "a step through a denied directory is refused");
        let fine = format!("{}/etc/./app.conf", tree.dir);
        assert_eq!(
            read_file(&tree.node, &path_args(&fine)).unwrap().data["path"],
            format!("{}/etc/app.conf", tree.dir)
        );
    }

    #[test]
    fn a_hard_link_is_not_read() {
        let tree = tree();
        std::fs::hard_link(format!("{}/outside/real", tree.dir), format!("{}/etc/hard", tree.dir)).unwrap();
        assert_eq!(refused(read_file(&tree.node, &path_args(&format!("{}/etc/hard", tree.dir)))), ErrorCode::Denied);
    }

    #[test]
    fn limens_own_logs_are_never_read() {
        let tree = tree();
        let node = Node::new(NodeConfig {
            allow: vec![format!("{}/**", tree.dir)],
            audit: format!("{}/etc/audit.jsonl", tree.dir),
            ..Default::default()
        });
        std::fs::write(&node.config.audit, "{}\n").unwrap();
        assert_eq!(refused(read_file(&node, &path_args(&node.config.audit))), ErrorCode::Denied);
    }

    #[test]
    fn grep_sees_only_what_redaction_left() {
        let tree = tree();
        let log = format!("{}/etc/app.log", tree.dir);
        std::fs::write(&log, "login ok\npassword=hunter2\n").unwrap();
        let matching_lines = |grep: &str| {
            let args = args_of(json!({"path": &log, "grep": grep}));
            read_file(&tree.node, &args).unwrap().data["lines"].as_array().unwrap().len()
        };
        assert_eq!(matching_lines("password=h"), 0, "a guess at the secret is not told apart");
        assert_eq!(matching_lines("password=[redacted]"), 1);
        assert_eq!(last_matching(vec!["token=x", "a", "A"], Some("a"), 1), ["A"]);
    }

    #[test]
    fn a_range_or_the_end_not_both() {
        let tree = tree();
        let path = format!("{}/etc/app.conf", tree.dir);
        let tail = read_file(&tree.node, &args_of(json!({"path": &path, "tail": 1}))).unwrap();
        assert_eq!(tail.data["lines"], json!(["password = \"[redacted]\""]));
        let both = read_file(&tree.node, &args_of(json!({"path": &path, "tail": 1, "from": 1})));
        assert_eq!(refused(both), ErrorCode::BadRequest);
    }

    #[test]
    fn listing_hides_what_cannot_be_read() {
        let tree = tree();
        let answer = list_dir(&tree.node, &path_args(&format!("{}/etc", tree.dir))).unwrap();
        let names: Vec<&str> =
            answer.data["entries"].as_array().unwrap().iter().map(|entry| entry["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"app.conf"), "{names:?}");
        assert!(!names.contains(&"private") && !names.contains(&"link"), "{names:?}");
    }

    #[test]
    fn a_file_holding_a_private_key_is_not_read_at_all() {
        // A window of lines between the markers would carry the key's body past redaction.
        let tree = tree();
        let window = args_of(json!({"path": format!("{}/etc/deploy.pem", tree.dir), "from": 2, "lines": 2}));
        assert_eq!(refused(read_file(&tree.node, &window)), ErrorCode::Denied);
    }

    #[test]
    fn a_key_in_a_big_log_is_masked_when_the_window_holds_it() {
        // Over the size searched whole for keys: the window is redacted as one text, markers and body together.
        let tree = tree();
        let filler = format!("{}\n", "x".repeat(99)).repeat(12_000);
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        std::fs::write(format!("{}/etc/big.log", tree.dir), format!("{filler}{key}done\n")).unwrap();
        let args = args_of(json!({"path": format!("{}/etc/big.log", tree.dir), "tail": 10}));
        let lines = read_file(&tree.node, &args).unwrap().data.to_string();
        assert!(lines.contains("done"), "{lines}");
        assert!(!lines.contains("b3BlbnNzaA"), "{lines}");
    }
}
