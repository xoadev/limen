//! What limen reads of `/etc` itself: accounts and groups for `list_dir`'s owners and for `install`, and the OS's name
//! for `hello`.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// `/etc/passwd`.
pub fn accounts(text: &str) -> Vec<Account> {
    text.lines().filter(|line| !line.starts_with('#')).filter_map(account).collect()
}

fn account(line: &str) -> Option<Account> {
    let fields: Vec<&str> = line.split(':').collect();
    let [name, _password, uid, gid, _gecos, home, shell, ..] = fields[..] else {
        return None;
    };
    Some(Account {
        name: name.into(),
        uid: uid.parse().ok()?,
        gid: gid.parse().unwrap_or(0),
        home: home.into(),
        shell: shell.into(),
    })
}

/// `/etc/group`: gid → name.
pub fn groups(text: &str) -> BTreeMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _password, gid, ..] = fields[..] else {
                return None;
            };
            Some((gid.parse().ok()?, name.to_string()))
        })
        .collect()
}

/// `PRETTY_NAME` of `/etc/os-release`.
pub fn os_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_groups() {
        let users =
            accounts("root:x:0:0:root:/root:/bin/bash\n# comment\nbroken\nlimen:x:998:998::/var/lib/limen:/bin/sh\n");
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].name, "limen");
        assert_eq!(users[1].uid, 998);
        assert_eq!(groups("root:x:0:\ndocker:x:999:ana\n")[&999], "docker");
    }

    #[test]
    fn os_name_of_os_release() {
        assert_eq!(
            os_name("NAME=x\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n").as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
    }
}
