//! Output cut into whole lines as it arrives, for a `run` with `follow` (spec §4): redaction sees whole lines, and a
//! private key's block whole.

const KEY_BEGIN: &str = "-----BEGIN ";
const PRIVATE_KEY_END: &str = "PRIVATE KEY-----";
const KEY_END: &str = "-----END ";

/// Bytes as they come, given back as whole lines once they are complete.
pub struct Lines {
    pending: Vec<u8>,
    /// What is held back for a line without its newline yet, or a key without its end: past it, it goes as it is.
    max_carry: usize,
    holds_keys: bool,
}

impl Lines {
    pub fn new(max_carry: usize) -> Self {
        Lines { pending: Vec::new(), max_carry, holds_keys: false }
    }

    /// Lines that hold back a private key's block from its `BEGIN` line until its `END` line, so the redactor finds
    /// it whole: in halves, its last short line would match no pattern.
    pub fn holding_private_keys(max_carry: usize) -> Self {
        Lines { holds_keys: true, ..Self::new(max_carry) }
    }

    /// The lines [bytes] completes, each ending in a newline; None while there is none to give.
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        let complete = self.pending.iter().rposition(|&byte| byte == b'\n').map_or(0, |newline| newline + 1);
        let given = if self.pending.len() > self.max_carry {
            self.pending.len()
        } else {
            complete - self.held_back(&self.pending[..complete])
        };
        let rest = self.pending.split_off(given);
        let lines = std::mem::replace(&mut self.pending, rest);
        (!lines.is_empty()).then(|| String::from_utf8_lossy(&lines).into_owned())
    }

    /// What is left at the end: a last line without its newline, or a key that never ended.
    pub fn rest(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.pending);
        (!rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }

    /// How many bytes at the end of [lines] belong to a private key whose block is still open.
    fn held_back(&self, lines: &[u8]) -> usize {
        if !self.holds_keys {
            return 0;
        }
        let text = String::from_utf8_lossy(lines);
        let Some(begin) = text.rfind(KEY_BEGIN) else { return 0 };
        let line_start = text[..begin].rfind('\n').map_or(0, |newline| newline + 1);
        let line_end = text[begin..].find('\n').map_or(text.len(), |newline| begin + newline);
        let opens_a_key = text[begin..line_end].trim_end().ends_with(PRIVATE_KEY_END);
        let ended = text[line_end..].contains(KEY_END);
        if opens_a_key && !ended { text.len() - line_start } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redactor::Redactor;

    #[test]
    fn whole_lines_only_and_the_rest_at_the_end() {
        let mut lines = Lines::new(1024);
        assert_eq!(lines.push(b"one\ntw"), Some("one\n".into()));
        assert_eq!(lines.push(b"o"), None);
        assert_eq!(lines.push(b"\nthree"), Some("two\n".into()));
        assert_eq!(lines.rest(), Some("three".into()));
    }

    #[test]
    fn a_line_longer_than_the_carry_goes_as_it_is() {
        let mut lines = Lines::new(8);
        assert_eq!(lines.push(b"0123"), None);
        assert_eq!(lines.push(b"456789"), Some("0123456789".into()));
        assert_eq!(lines.rest(), None);
    }

    #[test]
    fn a_private_key_cut_across_pieces_is_still_redacted() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                   b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
                   QyNTUxOQAAACDhortsecretshort\n\
                   -----END OPENSSH PRIVATE KEY-----\n";
        let (first, second) = key.split_at(key.find("QyNT").expect("the key has a second body line"));
        let redactor = Redactor::new(&[], &[]);
        let mut lines = Lines::holding_private_keys(64 * 1024);
        let mut sent = String::new();
        for piece in [format!("before\n{first}"), second.to_string(), "after\n".into()] {
            if let Some(text) = lines.push(piece.as_bytes()) {
                sent.push_str(&redactor.redact(&text));
            }
        }
        assert!(sent.starts_with("before\n") && sent.ends_with("after\n"), "{sent}");
        assert!(!sent.contains("hortsecretshort"), "the key's last line leaked: {sent}");
    }

    #[test]
    fn a_public_key_or_a_closed_block_is_not_held() {
        let mut lines = Lines::holding_private_keys(1024);
        assert_eq!(
            lines.push(b"-----BEGIN CERTIFICATE-----\nMIIB\n"),
            Some("-----BEGIN CERTIFICATE-----\nMIIB\n".into())
        );
        let closed = "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(lines.push(closed.as_bytes()), Some(closed.into()));
    }
}
