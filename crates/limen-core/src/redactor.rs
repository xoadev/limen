//! Masks secrets in text going out of the node: files, scripts' output, the audit log (spec §7.1). A pattern
//! with a group named `secret` replaces only that group, so `password=hunter2` stays readable as
//! `password=[redacted]`; without it the whole match goes.
//!
//! Best-effort by nature: the protection is not allowing files that hold secrets. This only catches the usual shapes.

use regex::{Captures, Regex};
use std::sync::OnceLock;

pub const MASK: &str = "[redacted]";

const SECRET_NAME: &str = "(?:password|passwd|passphrase|pwd|secret|token|psk|api[_-]?key|access[_-]?key|private[_-]?key|\
     preshared[_-]?key|client[_-]?secret)";

/// OpenWrt's UCI options that hold a secret: `option key '…'` is the Wi-Fi key.
const UCI_SECRET: &str =
    "(?:key|psk|private_key|preshared_key|priv_key_pwd|[a-z0-9_]*(?:password|passwd|secret|token))";

/// Whitespace and its opposite, ASCII: Unicode's classes cost milliseconds to compile, on every request.
const SPACE: &str = r"[\t\n\x0B\x0C\r ]";
const NOT_SPACE: &str = r"[^\t\n\x0B\x0C\r ]";

/// Tokens their provider gives a shape of its own, masked wherever they appear, named or not: GitHub's, JWTs, Slack's,
/// AWS access key ids, Stripe's secret keys and Google API keys. Joined into one pattern: one pass over the text.
const PROVIDER_TOKENS: [&str; 7] = [
    r"gh[pousr]_[A-Za-z0-9]{36,}",
    r"github_pat_[A-Za-z0-9_]{22,}",
    r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    r"xox[abprs]-[A-Za-z0-9-]{10,}",
    r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)",
    r"(?-u:\b)[sr]k_live_[A-Za-z0-9]{16,}",
    r"AIza[0-9A-Za-z_-]{35}",
];

/// The value assigned to a name [name] matches: `key = value`, `key: value`, `--key=value`, `"key": "value"`. A quoted
/// value is masked to its closing quote (or the end of the line), spaces included; a bare one to the next space or
/// separator.
fn assignments(name: &str) -> [String; 3] {
    let assignment = format!(r#"(?i){name}["']?{SPACE}*[:=]{SPACE}*"#);
    [
        format!(r#"{assignment}"(?<secret>[^"\n]*)"#),
        format!(r#"{assignment}'(?<secret>[^'\n]*)"#),
        format!(r#"{assignment}(?<secret>[^\t\n\x0B\x0C\r "',;&]+)"#),
    ]
}

/// The patterns of `redact.names`: the value assigned to any of [names], a whole name and not part of a longer one.
pub fn of_names(names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return vec![];
    }
    let escaped: Vec<String> = names.iter().map(|name| regex::escape(name)).collect();
    assignments(&format!("(?:(?m:^)|[^A-Za-z0-9_])(?:{})", escaped.join("|"))).to_vec()
}

pub fn built_in() -> Vec<String> {
    let mut patterns = assignments(SECRET_NAME).to_vec();
    patterns.extend([
        // --password value: a flag and its value, apart. The character before it is matched, not looked behind.
        format!(
            r#"(?i)(?:^|[^A-Za-z0-9_-])--?{SECRET_NAME}{SPACE}+(?<secret>[^\t\n\x0B\x0C\r "'-][^\t\n\x0B\x0C\r "']*)"#
        ),
        format!(r"(?i)authorization:{SPACE}*(?:bearer|basic|token){SPACE}+(?<secret>{NOT_SPACE}+)"),
        // UCI: option key 'value', quoted or not.
        format!(r#"(?m)^[\t ]*option[\t ]+{UCI_SECRET}[\t ]+['"]?(?<secret>[^'"\n]*)"#),
        // Passwords some commands take in their own way: curl -u user:pass, sshpass -p pass, mysql -ppass.
        r#"(?-u:\b)curl(?-u:\b)[^\n]*?[\t ](?:-u|--user)(?:[\t ]+|=)['"]?[^\t\n :'"]*:(?<secret>[^\t\n '"]+)"#.into(),
        r#"(?-u:\b)sshpass(?-u:\b)[^\n]*?[\t ]-p[\t ]*(?<secret>[^\t\n '"-][^\t\n '"]*)"#.into(),
        r#"(?-u:\b)mysql(?:dump|admin)?(?-u:\b)[^\n]*?[\t ]-p(?<secret>[^\t\n '"]+)"#.into(),
        // Credentials inside a URL: scheme://user:password@host.
        r"[a-zA-Z][a-zA-Z0-9+.-]*://[^/\t\n\x0B\x0C\r :@]+:(?<secret>[^@\t\n\x0B\x0C\r /]+)@".into(),
        format!("(?:{})", PROVIDER_TOKENS.join("|")),
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?s:.)*?-----END [A-Z ]*PRIVATE KEY-----".into(),
        // A line of base64 alone, as a key's body is written: a window of lines that starts inside a key has no
        // markers to find it by.
        r"(?m)^(?<body>[A-Za-z0-9+/]{64,})={0,2}\r?$".into(),
    ]);
    patterns
}

/// Compiled on the first text it redacts: most requests redact nothing, and every request is a process of its own.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    extra: Vec<String>,
    patterns: OnceLock<Vec<Regex>>,
}

impl Redactor {
    /// The built-in patterns, the operator's `redact.names` and `redact.patterns`, which reading the configuration
    /// already checked: one that doesn't compile is left out rather than stop every answer.
    pub fn new(names: &[String], patterns: &[String]) -> Self {
        let mut extra = of_names(names);
        extra.extend_from_slice(patterns);
        Self { extra, patterns: OnceLock::new() }
    }

    fn patterns(&self) -> &[Regex] {
        self.patterns.get_or_init(|| {
            built_in().iter().chain(&self.extra).filter_map(|pattern| Regex::new(pattern).ok()).collect()
        })
    }

    pub fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_string();
        for regex in self.patterns() {
            redacted = regex.replace_all(&redacted, masked).into_owned();
        }
        redacted
    }
}

/// A match with its `secret` group masked, or all of it when the pattern has no such group.
fn masked(captures: &Captures) -> String {
    let whole = captures.get(0).expect("group 0 is the whole match");
    // A line of hexadecimal alone is a hash or an ID —a container's, an image's—, not a key's body: 64 characters of
    // base64 that happen to all be hexadecimal don't occur.
    if captures.name("body").is_some_and(|body| body.as_str().bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return whole.as_str().to_string();
    }
    match captures.name("secret") {
        Some(secret) => {
            let text = whole.as_str();
            let before = &text[..secret.start() - whole.start()];
            let after = &text[secret.end() - whole.start()..];
            format!("{before}{MASK}{after}")
        }
        None => MASK.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(text: &str) -> String {
        Redactor::default().redact(text)
    }

    #[test]
    fn keeps_the_key_and_masks_the_value() {
        assert_eq!(redact("password=hunter2 user=ana"), "password=[redacted] user=ana");
        assert_eq!(redact("DB_PASSWORD: s3cr3t"), "DB_PASSWORD: [redacted]");
        assert_eq!(redact("--api-key=abc123 --verbose"), "--api-key=[redacted] --verbose");
        assert_eq!(redact(r#""token": "eyJhbGc""#), r#""token": "[redacted]""#);
    }

    #[test]
    fn a_quoted_value_goes_whole_spaces_included() {
        assert_eq!(redact("password = \"correct horse\"\nuser = \"ana\""), "password = \"[redacted]\"\nuser = \"ana\"");
        assert_eq!(redact("secret: 'two words' # x"), "secret: '[redacted]' # x");
        assert_eq!(redact(r#"{"password":"a b","user":"ana"}"#), r#"{"password":"[redacted]","user":"ana"}"#);
        // A line cut before the closing quote still loses the value.
        assert_eq!(redact("token=\"abc def"), "token=\"[redacted]");
    }

    #[test]
    fn a_flag_and_its_value_apart() {
        // As /proc/<pid>/cmdline reads, arguments joined by spaces.
        assert_eq!(redact("app --password hunter2 --user ana"), "app --password [redacted] --user ana");
        assert_eq!(redact("app -token abc"), "app -token [redacted]");
        assert_eq!(redact("--password hunter2"), "--password [redacted]");
        assert_eq!(redact("app --token-file /etc/app/token"), "app --token-file /etc/app/token");
        assert_eq!(redact("the token was refreshed"), "the token was refreshed");
    }

    #[test]
    fn headers_urls_and_keys() {
        assert_eq!(redact("Authorization: Bearer eyJ.abc.def"), "Authorization: Bearer [redacted]");
        assert_eq!(redact("postgres://app:pa55@db:5432/x"), "postgres://app:[redacted]@db:5432/x");
        let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz";
        assert_eq!(redact(pem), "a\n[redacted]\nz");
    }

    #[test]
    fn openwrt_and_the_usual_tools() {
        assert_eq!(
            redact("config wifi-iface\n\toption ssid 'home'\n\toption key 'hunter22'"),
            "config wifi-iface\n\toption ssid 'home'\n\toption key '[redacted]'"
        );
        assert_eq!(redact("\toption private_key \"wgkey=\""), "\toption private_key \"[redacted]\"");
        assert_eq!(redact("\toption password s3cr3t"), "\toption password [redacted]");
        assert_eq!(redact("wpa_passphrase=hunter22\npsk=\"abc def\""), "wpa_passphrase=[redacted]\npsk=\"[redacted]\"");
        assert_eq!(redact("PresharedKey = abc="), "PresharedKey = [redacted]");
        assert_eq!(redact("curl -s -u ana:pa55 https://x"), "curl -s -u ana:[redacted] https://x");
        assert_eq!(redact("sshpass -p pa55 ssh host"), "sshpass -p [redacted] ssh host");
        assert_eq!(redact("mysql -u root -ppa55 db"), "mysql -u root -p[redacted] db");
        // Not every -u or -p is a password.
        assert_eq!(redact("docker run -u 1000:1000 -p 80:80 app"), "docker run -u 1000:1000 -p 80:80 app");
    }

    #[test]
    fn a_key_body_without_its_markers() {
        let body = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW";
        assert_eq!(
            redact(&format!("{body}\n{body}\n-----END OPENSSH PRIVATE KEY-----")),
            "[redacted]\n[redacted]\n-----END OPENSSH PRIVATE KEY-----"
        );
        assert_eq!(redact("short line of text"), "short line of text");
        // What `docker system prune` lists, a container's full ID, is not a key.
        let container = "0e64f2360a448b389c83fcbc3705e1364ccf43f004bb55233c689e7ce59a1ae9";
        assert_eq!(
            redact(&format!("Deleted Containers:\n{container}\n")),
            format!("Deleted Containers:\n{container}\n")
        );
    }

    /// [prefix] and then [length] alphanumerics, so no token is spelled out whole in this file.
    fn with_tail(prefix: &str, length: usize) -> String {
        format!("{prefix}{}", "a1B2".chars().cycle().take(length).collect::<String>())
    }

    #[test]
    fn provider_tokens_are_masked_wherever_they_appear() {
        let tokens = [
            with_tail("ghp_", 36),
            with_tail("gho_", 36),
            with_tail("ghu_", 40),
            with_tail("ghs_", 36),
            with_tail("ghr_", 36),
            with_tail("github_pat_", 22),
            format!("{}_{}", with_tail("github_pat_11", 22), with_tail("", 40)),
            format!("eyJ{0}.eyJ{0}.{0}-_", "a1B2c3D4"),
            with_tail("xoxb-", 10),
            "xoxp-1234567890-1234567890-abcdefABCDEF".into(),
            "xoxa-".to_string() + &"1".repeat(12),
            "AKIAIOSFODNN7EXAMPLE".into(),
            "ASIAIOSFODNN7EXAMPLE".into(),
            with_tail("sk_live_", 16),
            with_tail("rk_live_", 30),
            with_tail("AIza", 35),
            format!("AIza{}-_", with_tail("", 33)),
        ];
        for token in tokens {
            assert_eq!(redact(&format!("value {token} end")), "value [redacted] end", "{token}");
            assert_eq!(redact(&token), "[redacted]", "{token}");
            assert_eq!(redact(&format!("a: {token}\nb: {token}")), "a: [redacted]\nb: [redacted]", "{token}");
        }
    }

    #[test]
    fn a_provider_token_in_an_assignment_or_a_url_is_masked_once() {
        let token = with_tail("ghp_", 40);
        assert_eq!(redact(&format!("GITHUB_TOKEN={token}")), "GITHUB_TOKEN=[redacted]");
        assert_eq!(redact(&format!("https://ana:{token}@github.com/x")), "https://ana:[redacted]@github.com/x");
        assert_eq!(
            redact(&format!("git clone https://{token}@github.com/x")),
            "git clone https://[redacted]@github.com/x"
        );
    }

    #[test]
    fn near_misses_of_provider_tokens_are_left_alone() {
        let near_misses = [
            with_tail("ghp_", 35),
            with_tail("ghx_", 40),
            with_tail("gh_", 40),
            with_tail("github_pat_", 21),
            format!("eyJ{0}.eyJ{0}", "a1B2c3D4"),
            format!("eyJ{0}.eyJ{0}.{1}", "a1B2c3D4", "short"),
            format!("eyJ{0}.abc.{0}", "a1B2c3D4"),
            with_tail("xoxb-", 9),
            with_tail("xoxz-", 12),
            "AKIAIOSFODNN7EXAMPL".into(),
            "AKIAIOSFODNN7EXAMPLES".into(),
            "XAKIAIOSFODNN7EXAMPLE".into(),
            "akiaiosfodnn7example".into(),
            "AKIA-IOSFODNN7EXAMPLE".into(),
            with_tail("sk_live_", 15),
            with_tail("sk_test_", 30),
            with_tail("pk_live_", 30),
            with_tail("disk_live_", 30),
            with_tail("AIza", 34),
            with_tail("AIzb", 35),
        ];
        for text in near_misses {
            let line = format!("id {text} end");
            assert_eq!(redact(&line), line);
        }
    }

    #[test]
    fn identifiers_are_not_tokens() {
        let text = format!(
            "container 123e4567-e89b-12d3-a456-426614174000 image sha256:{} commit 9fceb02d0ae598e95dc970b74767f19372d61af8",
            "ab12".repeat(16)
        );
        assert_eq!(redact(&text), text);
    }

    #[test]
    fn every_built_in_pattern_compiles() {
        for pattern in built_in() {
            Regex::new(&pattern).unwrap_or_else(|error| panic!("{pattern}: {error}"));
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let text = "Started nginx.service - A high performance web server.";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn extra_patterns_with_and_without_group() {
        let redactor = Redactor::new(&[], &["sk-[A-Za-z0-9]{8,}".into(), r"pin (?<secret>\d{4})".into()]);
        assert_eq!(redactor.redact("key sk-abcdefgh123 and pin 1234"), "key [redacted] and pin [redacted]");
    }

    #[test]
    fn the_operators_names() {
        let redactor = Redactor::new(&["MQTT_PASS".into(), "ha.key".into()], &[]);
        assert_eq!(redactor.redact("MQTT_PASS=abc MQTT_USER=ana"), "MQTT_PASS=[redacted] MQTT_USER=ana");
        assert_eq!(redactor.redact(r#"{"mqtt_pass": "a b"}"#), r#"{"mqtt_pass": "[redacted]"}"#);
        assert_eq!(redactor.redact("ha.key: x1"), "ha.key: [redacted]");
        // A whole name: not the end of a longer one, and a dot is a dot.
        assert_eq!(redactor.redact("OLD_MQTT_PASS=abc haXkey=1"), "OLD_MQTT_PASS=abc haXkey=1");
    }
}
