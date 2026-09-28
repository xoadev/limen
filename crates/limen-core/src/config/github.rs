//! The link `install` prints to create the repository token (spec §10): GitHub's template URL for fine-grained
//! tokens, filled with a name, the owner, no expiry and read access to contents. The repository itself can't be
//! chosen by URL; the form asks for it.

pub fn token_url(owner: &str, repo: &str, host: &str) -> String {
    let name: String = format!("limen-{host}").chars().take(40).collect();
    let params = [
        ("name", name),
        ("description", format!("limen on {host}: read {owner}/{repo}")),
        ("target_name", owner.to_string()),
        ("expires_in", "none".into()),
        ("contents", "read".into()),
    ];
    let query: Vec<String> = params.iter().map(|(key, value)| format!("{key}={}", encode(value))).collect();
    format!("https://github.com/settings/personal-access-tokens/new?{}", query.join("&"))
}

/// [text] percent-encoded: every byte but URLs' unreserved characters.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(byte).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_filled_in_form() {
        let url = super::token_url("you", "infra", "nas");
        assert_eq!(
            url,
            "https://github.com/settings/personal-access-tokens/new?name=limen-nas&description=limen%20on%20nas%3A%20read%20you%2Finfra&target_name=you&expires_in=none&contents=read"
        );
    }
}
