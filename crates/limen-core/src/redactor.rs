//! Masks secrets in text going out of the node: files, logs, check output, process arguments (spec §7.1). A pattern
//! with a group named `secret` replaces only that group, so `password=hunter2` stays readable as
//! `password=[redacted]`; without it the whole match goes.
//!
//! Best-effort by nature: the protection is not allowing files that hold secrets. This only catches the usual shapes.

use regex::{Captures, Regex};
use std::sync::OnceLock;

pub const MASK: &str = "[redacted]";

const SECRET_NAME: &str =
    "(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret)";

/// Whitespace and its opposite, ASCII: Unicode's classes cost milliseconds to compile, on every request.
const SPACE: &str = r"[\t\n\x0B\x0C\r ]";
const NOT_SPACE: &str = r"[^\t\n\x0B\x0C\r ]";

pub fn built_in() -> Vec<String> {
    let s = SPACE;
    vec![
        // key = value, key: value, --key=value, "key": "value", for the usual names of secrets. A quoted value is
        // masked to its closing quote (or the end of the line), spaces included; a bare one to the next space or
        // separator.
        format!(r#"(?i){SECRET_NAME}["']?{s}*[:=]{s}*"(?<secret>[^"\n]*)"#),
        format!(r#"(?i){SECRET_NAME}["']?{s}*[:=]{s}*'(?<secret>[^'\n]*)"#),
        format!(r#"(?i){SECRET_NAME}["']?{s}*[:=]{s}*(?<secret>[^\t\n\x0B\x0C\r "',;&]+)"#),
        // --password value: a flag and its value, apart. The character before it is matched, not looked behind.
        format!(r#"(?i)(?:^|[^A-Za-z0-9_-])--?{SECRET_NAME}{s}+(?<secret>[^\t\n\x0B\x0C\r "'-][^\t\n\x0B\x0C\r "']*)"#),
        format!(r"(?i)authorization:{s}*(?:bearer|basic|token){s}+(?<secret>{NOT_SPACE}+)"),
        // Credentials inside a URL: scheme://user:password@host.
        r"[a-zA-Z][a-zA-Z0-9+.-]*://[^/\t\n\x0B\x0C\r :@]+:(?<secret>[^@\t\n\x0B\x0C\r /]+)@".into(),
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?s:.)*?-----END [A-Z ]*PRIVATE KEY-----".into(),
    ]
}

/// Compiled on the first text it redacts: most requests redact nothing, and every request is a process of its own.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    extra: Vec<String>,
    patterns: OnceLock<Vec<Regex>>,
}

impl Redactor {
    /// The built-in patterns and [extra], the operator's `redact.patterns`, which reading the configuration already
    /// checked: one that doesn't compile is left out rather than stop every answer.
    pub fn new(extra: &[String]) -> Self {
        Self { extra: extra.to_vec(), patterns: OnceLock::new() }
    }

    fn patterns(&self) -> &[Regex] {
        self.patterns.get_or_init(|| built_in().iter().chain(&self.extra).filter_map(|p| Regex::new(p).ok()).collect())
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for regex in self.patterns() {
            out = regex
                .replace_all(&out, |caps: &Captures| {
                    let whole = caps.get(0).unwrap();
                    match caps.name("secret") {
                        Some(secret) => format!(
                            "{}{MASK}{}",
                            &whole.as_str()[..secret.start() - whole.start()],
                            &whole.as_str()[secret.end() - whole.start()..]
                        ),
                        None => MASK.to_string(),
                    }
                })
                .into_owned();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(text: &str) -> String {
        Redactor::default().redact(text)
    }

    #[test]
    fn keeps_the_key_and_masks_the_value() {
        assert_eq!(r("password=hunter2 user=ana"), "password=[redacted] user=ana");
        assert_eq!(r("DB_PASSWORD: s3cr3t"), "DB_PASSWORD: [redacted]");
        assert_eq!(r("--api-key=abc123 --verbose"), "--api-key=[redacted] --verbose");
        assert_eq!(r(r#""token": "eyJhbGc""#), r#""token": "[redacted]""#);
    }

    #[test]
    fn a_quoted_value_goes_whole_spaces_included() {
        assert_eq!(r("password = \"correct horse\"\nuser = \"ana\""), "password = \"[redacted]\"\nuser = \"ana\"");
        assert_eq!(r("secret: 'two words' # x"), "secret: '[redacted]' # x");
        assert_eq!(r(r#"{"password":"a b","user":"ana"}"#), r#"{"password":"[redacted]","user":"ana"}"#);
        // A line cut before the closing quote still loses the value.
        assert_eq!(r("token=\"abc def"), "token=\"[redacted]");
    }

    #[test]
    fn a_flag_and_its_value_apart() {
        // As /proc/<pid>/cmdline reads, arguments joined by spaces.
        assert_eq!(r("app --password hunter2 --user ana"), "app --password [redacted] --user ana");
        assert_eq!(r("app -token abc"), "app -token [redacted]");
        assert_eq!(r("--password hunter2"), "--password [redacted]");
        assert_eq!(r("app --token-file /etc/app/token"), "app --token-file /etc/app/token");
        assert_eq!(r("the token was refreshed"), "the token was refreshed");
    }

    #[test]
    fn headers_urls_and_keys() {
        assert_eq!(r("Authorization: Bearer eyJ.abc.def"), "Authorization: Bearer [redacted]");
        assert_eq!(r("postgres://app:pa55@db:5432/x"), "postgres://app:[redacted]@db:5432/x");
        let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz";
        assert_eq!(r(pem), "a\n[redacted]\nz");
    }

    #[test]
    fn every_built_in_pattern_compiles() {
        for p in built_in() {
            Regex::new(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let text = "Started nginx.service - A high performance web server.";
        assert_eq!(r(text), text);
    }

    #[test]
    fn extra_patterns_with_and_without_group() {
        let red = Redactor::new(&["sk-[A-Za-z0-9]{8,}".into(), r"pin (?<secret>\d{4})".into()]);
        assert_eq!(red.redact("key sk-abcdefgh123 and pin 1234"), "key [redacted] and pin [redacted]");
    }
}
