//! Whether a script may run (spec §6): the file and every directory above it owned by root or by the user limen runs
//! as —root in production— and not writable by group or others: `sshd`'s `StrictModes` rule. Otherwise whoever can
//! write there can make the gate run anything as root.

/// What limen needs to know of a file or directory to decide whether to trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub uid: u32,
    pub mode: u32,
    pub is_directory: bool,
    pub is_regular: bool,
}

const GROUP_OR_OTHER_WRITE: u32 = 0o022;
const OWNER_EXECUTE: u32 = 0o100;

/// [chain] is the script first, then each parent up to `/`. None when trusted, otherwise the reason.
pub fn problem(chain: &[FileStat], owner: u32) -> Option<String> {
    let Some(file) = chain.first() else {
        return Some("not found".into());
    };
    if !file.is_regular {
        return Some(format!("{} is not a regular file", file.path));
    }
    if file.mode & OWNER_EXECUTE == 0 {
        return Some(format!("{} is not executable", file.path));
    }
    for entry in chain {
        if entry.uid != 0 && entry.uid != owner {
            let who = if owner == 0 { "root".to_string() } else { format!("root or uid {owner}") };
            return Some(format!("{} is not owned by {who}", entry.path));
        }
        if entry.mode & GROUP_OR_OTHER_WRITE != 0 {
            return Some(format!("{} is writable by group or others", entry.path));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(path: &str, uid: u32, mode: u32, dir: bool) -> FileStat {
        FileStat { path: path.into(), uid, mode, is_directory: dir, is_regular: !dir }
    }

    #[test]
    fn follows_strict_modes() {
        let script = stat("/etc/limen/checks.d/a", 0, 0o755, false);
        let parents: Vec<FileStat> =
            ["/etc/limen/checks.d", "/etc/limen", "/etc", "/"].iter().map(|p| stat(p, 0, 0o755, true)).collect();
        let chain = |first: FileStat, parents: &[FileStat]| [vec![first], parents.to_vec()].concat();
        assert_eq!(problem(&chain(script.clone(), &parents), 0), None);
        let mut foreign = script.clone();
        foreign.uid = 1000;
        assert_eq!(
            problem(&chain(foreign.clone(), &parents), 0).unwrap(),
            "/etc/limen/checks.d/a is not owned by root"
        );
        // Run as a user, that user's files are trusted too, and root's parents still are.
        assert_eq!(problem(&chain(foreign.clone(), &parents), 1000), None);
        foreign.uid = 1001;
        assert_eq!(
            problem(&chain(foreign, &parents), 1000).unwrap(),
            "/etc/limen/checks.d/a is not owned by root or uid 1000"
        );
        let mut loose = parents.clone();
        loose[1].mode = 0o775;
        assert_eq!(problem(&chain(script.clone(), &loose), 0).unwrap(), "/etc/limen is writable by group or others");
        let mut foreign_parent = parents.clone();
        foreign_parent[1].uid = 1000;
        assert_eq!(problem(&chain(script.clone(), &foreign_parent), 0).unwrap(), "/etc/limen is not owned by root");
        let mut plain = script;
        plain.mode = 0o644;
        assert_eq!(problem(&chain(plain, &parents), 0).unwrap(), "/etc/limen/checks.d/a is not executable");
    }
}
