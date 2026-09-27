//! `limen install`, `uninstall` and `token` (spec §3, §10): the wiring that makes the gate the only way in, and the
//! repository the node takes its scripts from. Every step says what it does; `--dry-run` says it without doing it,
//! and running it twice changes nothing the second time.
//!
//! Two wirings. Debian and the like: OpenSSH, one system user per role and a sudo rule for exactly its gate. OpenWrt:
//! dropbear, which has neither sudo nor extra users, so both keys belong to root, each held to its gate by its forced
//! command.

use super::{Node, repo, system};
use crate::os::{fs, proc, sys};
use limen_core::config::github;
use limen_core::config::node::{self as node_config, NodeConfig, RepoConfig};
use limen_core::requests::Role;
use regex::Regex;
use std::cell::Cell;
use std::time::Duration;

pub const SUDOERS: &str = "/etc/sudoers.d/limen";
pub const DROPBEAR_KEYS: &str = "/etc/dropbear/authorized_keys";
pub const KEEP: &str = "/lib/upgrade/keep.d/limen";
/// What marks limen's lines in a shared authorized_keys: its forced command.
const LIMEN_LINE: &str = "limen gate --role";
const PUBLIC_KEY: &str = r#"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com|sk-ecdsa-sha2-nistp256@openssh\.com) [A-Za-z0-9+/]+=*( [^\r\n"]*)?$"#;
const CIDRS: &str = "^[0-9A-Fa-f.:/*?,!]+$";

pub type Outcome<T> = Result<T, String>;

/// `--repo`, `--branch` and `--path` of `install`.
#[derive(Debug, Clone)]
pub struct RepoOptions {
    pub url: String,
    pub branch: String,
    pub path: String,
}

pub fn user_of(role: Role) -> String {
    format!("limen-{}", role.wire())
}

pub struct Installer {
    dry_run: bool,
    warnings: Cell<u32>,
    openwrt: bool,
    binary: &'static str,
}

impl Installer {
    pub fn new(dry_run: bool) -> Self {
        let openwrt = system::openwrt();
        Installer {
            dry_run,
            warnings: Cell::new(0),
            openwrt,
            binary: if openwrt { "/usr/bin/limen" } else { "/usr/local/bin/limen" },
        }
    }

    /// Who the hub logs in as: the read role's user, or root on OpenWrt.
    pub fn read_user(&self) -> String {
        if self.openwrt { "root".into() } else { user_of(Role::Read) }
    }

    pub fn install(
        &self,
        read_key: &str,
        deploy_key: Option<&str>,
        from: Option<&str>,
        repo: Option<&RepoOptions>,
        announce: bool,
    ) -> Outcome<i32> {
        validate_key("--read-key", read_key)?;
        if let Some(k) = deploy_key {
            validate_key("--deploy-key", k)?;
        }
        if let Some(f) = from {
            if !Regex::new(CIDRS).unwrap().is_match(f) {
                return Err(format!("--from '{f}' is not a list of addresses or CIDRs"));
            }
            if self.openwrt {
                return Err(
                    "dropbear has no from= option; limit port 22 with the firewall and run it without --from".into()
                );
            }
        }
        let repo_config = repo.map(repo_config).transpose()?;
        self.require_root()?;
        if repo_config.is_some() && proc::which("git").is_none() {
            let how = if self.openwrt { "the git-http package" } else { "apt install git" };
            return Err(format!("--repo needs git on this node ({how})"));
        }
        // Access to the repository first, token included: a repository that can't be read stops the install before
        // anything on the machine has changed.
        if let Some(r) = &repo_config {
            if !self.dry_run {
                self.ask_token(r, false)?;
            }
        }
        self.install_binary()?;
        let mut keys = vec![(Role::Read, read_key)];
        if let Some(k) = deploy_key {
            keys.push((Role::Deploy, k));
        }
        if self.openwrt {
            self.write_dropbear_keys(&keys)?;
            self.write_sysupgrade_keep()?;
            self.check_dropbear();
        } else {
            for (role, key) in &keys {
                self.ensure_user(&user_of(*role))?;
                self.write_authorized_keys(&user_of(*role), *role, key, from)?;
            }
            if deploy_key.is_none() && fs::account(&user_of(Role::Deploy)).is_some() {
                self.note(&format!(
                    "{} exists from an earlier install; left as it is (uninstall removes it)",
                    user_of(Role::Deploy)
                ));
            }
            self.write_sudoers(&keys.iter().map(|(r, _)| *r).collect::<Vec<_>>())?;
            self.check_sshd(&keys.iter().map(|(r, _)| user_of(*r)).collect::<Vec<_>>());
        }
        self.ensure_directories(repo)?;
        if let Some(r) = &repo_config {
            self.connect_repo(r)?;
        }
        if !announce {
            return Ok(0);
        }
        say("");
        say(if self.dry_run { "Dry run: nothing was changed." } else { "limen is installed." });
        say(&format!(
            "Next: list what may be read in {} ([files].allow is empty), then add this node to the hub:",
            node_config::PATH
        ));
        let name: String =
            sys::hostname().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        say(&format!("  [nodes.{name}]"));
        say("  host = \"<address>\"");
        if self.openwrt {
            say("  user = \"root\"");
        }
        say(&format!(
            "  host_key = \"{}\"",
            self.host_key().unwrap_or("<ssh-keyscan -t ed25519 this-host, checked out of band>".into())
        ));
        if self.warnings.get() > 0 {
            say(&format!("{} warning(s) above.", self.warnings.get()));
        }
        Ok(0)
    }

    pub fn uninstall(&self, purge: bool) -> Outcome<i32> {
        self.require_root()?;
        let config = Node::load(node_config::PATH).ok().map(|n| n.config);
        if self.openwrt {
            self.remove_dropbear_keys()?;
            if fs::exists(KEEP) {
                self.act(&format!("remove {KEEP}"), || {
                    fs::remove(KEEP);
                    Ok(())
                })?;
            }
        } else {
            for role in [Role::Read, Role::Deploy] {
                let user = user_of(role);
                if fs::account(&user).is_some() {
                    self.act(&format!("remove user {user} and its home"), || exec(&["userdel", "--remove", &user]))?;
                }
            }
            if fs::exists(SUDOERS) {
                self.act(&format!("remove {SUDOERS}"), || {
                    fs::remove(SUDOERS);
                    Ok(())
                })?;
            }
        }
        if purge {
            let repo = config.as_ref().and_then(|c| c.repo.clone());
            let mut paths = vec!["/etc/limen".to_string(), "/var/log/limen".to_string()];
            if let Some(r) = &repo {
                paths.extend([r.dir.clone(), r.sync_record()]);
            }
            for path in paths.iter().filter(|p| fs::exists(p)) {
                self.act(&format!("remove {path}"), || exec(&["rm", "-rf", "--", path]))?;
            }
        } else {
            self.note("kept /etc/limen, the logs and the repository checkout (--purge removes them)");
        }
        if fs::exists(self.binary) {
            self.act(&format!("remove {}", self.binary), || {
                fs::remove(self.binary);
                Ok(())
            })?;
        }
        say(if self.dry_run { "Dry run: nothing was changed." } else { "limen is uninstalled." });
        Ok(0)
    }

    /// `limen token`: asks for the repository token again, checks it and saves it. For when it expires.
    pub fn token(&self) -> Outcome<i32> {
        self.require_root()?;
        let node = Node::load(node_config::PATH).map_err(|e| e.message)?;
        let repo = node.config.repo.ok_or(format!("no [repo] in {}", node_config::PATH))?;
        self.ask_token(&repo, true)?;
        Ok(0)
    }

    /// The repository: a token when it needs one, and the first checkout.
    fn connect_repo(&self, repo: &RepoConfig) -> Outcome<()> {
        if self.dry_run {
            say(&format!(
                "would check access to {}, ask for a token if it needs one, and sync it to {}",
                repo.display_url(),
                repo.dir
            ));
            return Ok(());
        }
        say(&format!("sync {} {} into {}", repo.display_url(), repo.branch, repo.dir));
        let node = Node::load(node_config::PATH).map_err(|e| e.message)?;
        let (_, to) = repo::sync(&node, repo).map_err(|e| format!("sync: {}", e.message))?;
        self.note(&format!("at {}; `limen apply` runs its setup scripts and stacks", &to[..to.len().min(12)]));
        Ok(())
    }

    fn ask_token(&self, repo: &RepoConfig, force: bool) -> Outcome<()> {
        if !force {
            match repo::access(repo, None) {
                repo::Access::Readable => {
                    self.note(&format!("{} is readable without a token", repo.display_url()));
                    return Ok(());
                }
                repo::Access::Failed(m) => return Err(format!("cannot reach {}: {m}", repo.display_url())),
                repo::Access::NeedsToken => {}
            }
            if let Some(saved) = repo::token(repo) {
                if matches!(repo::access(repo, Some(&saved)), repo::Access::Readable) {
                    self.note(&format!("the saved token reads {}", repo.display_url()));
                    return Ok(());
                }
            }
        }
        if !repo.url.starts_with("https://") {
            return Err(format!("{} is not readable, and a token only works over https://", repo.display_url()));
        }
        say("");
        say(&format!("{} needs a token that can read it.", repo.display_url()));
        if let Some((owner, name)) = repo.github() {
            say("Create one here (fine-grained, read-only contents, no expiry):");
            say(&format!("  {}", github::token_url(&owner, &name, &sys::hostname())));
            say(&format!(
                "In the form, check that the resource owner is {owner} (select it again if in doubt) and, under"
            ));
            say(&format!("Repository access, choose \"Only select repositories\" and {name}."));
        }
        for _ in 0..3 {
            sys::err("Token: ");
            let token = if sys::stdin_is_terminal() { sys::read_secret() } else { sys::read_line() };
            let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).ok_or("no token given")?;
            match repo::access(repo, Some(&token)) {
                repo::Access::Failed(m) => return Err(format!("cannot reach {}: {m}", repo.display_url())),
                repo::Access::Readable => {
                    let dir = repo.token_file.rsplit_once('/').map_or("/", |(d, _)| d);
                    fs::mkdirs(dir, 0o755)?;
                    fs::write_atomic(&repo.token_file, format!("{token}\n").as_bytes(), 0o600)?;
                    fs::chown(&repo.token_file, 0, 0)?;
                    say(&format!("token saved in {} (root only)", repo.token_file));
                    return Ok(());
                }
                repo::Access::NeedsToken => say(&format!("that token can't read {}", repo.display_url())),
            }
        }
        Err(format!("no token that reads {}", repo.display_url()))
    }

    fn install_binary(&self) -> Outcome<()> {
        let me = fs::real_path("/proc/self/exe").ok_or("cannot find this binary")?;
        if me == self.binary {
            return Ok(());
        }
        let bytes = fs::read(&me, usize::MAX).ok_or(format!("cannot read {me}"))?;
        if fs::real_path(self.binary).and_then(|p| fs::read(&p, usize::MAX)).as_ref() == Some(&bytes) {
            return Ok(());
        }
        self.act(&format!("install this binary as {}", self.binary), || {
            fs::mkdirs(self.binary.rsplit_once('/').map_or("/", |(d, _)| d), 0o755)?;
            fs::write_atomic(self.binary, &bytes, 0o755)?;
            fs::chown(self.binary, 0, 0)
        })
    }

    fn ensure_user(&self, user: &str) -> Outcome<()> {
        let home = format!("/var/lib/{user}");
        let existing = fs::account(user);
        if existing.is_none() {
            self.act(&format!("create system user {user} (home {home}, shell /bin/sh)"), || {
                exec(&[
                    "useradd",
                    "--system",
                    "--shell",
                    "/bin/sh",
                    "--home-dir",
                    &home,
                    "--create-home",
                    "--user-group",
                    user,
                ])
            })?;
        }
        // `/bin/sh` and not `nologin`: sshd runs the forced command through the login shell, and with `nologin`
        // nothing runs. `*` and not `!`: a locked password makes sshd refuse even key logins when PAM is off.
        if existing.as_ref().is_none_or(|a| a.shell != "/bin/sh") || shadow_password(user).as_deref() != Some("*") {
            self.act(&format!("set {user}'s shell to /bin/sh and its password to none (key login only)"), || {
                exec(&["usermod", "--shell", "/bin/sh", "--password", "*", user])
            })?;
        }
        Ok(())
    }

    fn write_authorized_keys(&self, user: &str, role: Role, key: &str, from: Option<&str>) -> Outcome<()> {
        let home = fs::account(user).map(|a| a.home).unwrap_or(format!("/var/lib/{user}"));
        let from = from.map(|f| format!(",from=\"{f}\"")).unwrap_or_default();
        let line =
            format!("restrict{from},command=\"sudo -n {} gate --role {}\" {}\n", self.binary, role.wire(), key.trim());
        let path = format!("{home}/.ssh/authorized_keys");
        if fs::read_following(&path).as_deref() == Some(line.as_str()) {
            return Ok(());
        }
        // Owned by root: the account itself can't change which key opens it or what that key runs.
        self.act(&format!("write {path} ({} role, forced command)", role.wire()), || {
            let ssh = format!("{home}/.ssh");
            fs::mkdirs(&ssh, 0o755)?;
            fs::chown(&ssh, 0, 0)?;
            fs::chmod(&ssh, 0o755)?;
            fs::write_following(&path, line.as_bytes(), 0o644)?;
            fs::chown(&path, 0, 0)
        })
    }

    /// Root's `authorized_keys` of dropbear, shared with whoever administers the router: only limen's own lines
    /// (those with its forced command) are replaced; every other key stays as it was.
    fn write_dropbear_keys(&self, keys: &[(Role, &str)]) -> Outcome<()> {
        let current = fs::read_following(DROPBEAR_KEYS);
        let mut lines: Vec<String> = current
            .as_deref()
            .unwrap_or("")
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.contains(LIMEN_LINE))
            .map(String::from)
            .collect();
        for (role, key) in keys {
            lines.push(format!(
                "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty,command=\"{} gate --role {}\" {}",
                self.binary,
                role.wire(),
                key.trim()
            ));
        }
        let text = format!("{}\n", lines.join("\n"));
        if current.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        let roles: Vec<&str> = keys.iter().map(|(r, _)| r.wire()).collect();
        self.act(
            &format!("write limen's keys in {DROPBEAR_KEYS} ({}, forced commands; other keys kept)", roles.join(", ")),
            || {
                fs::mkdirs("/etc/dropbear", 0o700)?;
                fs::write_following(DROPBEAR_KEYS, text.as_bytes(), 0o600)
            },
        )
    }

    fn remove_dropbear_keys(&self) -> Outcome<()> {
        let Some(current) = fs::read_following(DROPBEAR_KEYS) else { return Ok(()) };
        let lines: Vec<&str> = current.lines().filter(|l| !l.trim().is_empty()).collect();
        let kept: Vec<&str> = lines.iter().copied().filter(|l| !l.contains(LIMEN_LINE)).collect();
        if kept.len() == lines.len() {
            return Ok(());
        }
        self.act(&format!("remove limen's keys from {DROPBEAR_KEYS} (other keys kept)"), || {
            let text = if kept.is_empty() { String::new() } else { format!("{}\n", kept.join("\n")) };
            fs::write_following(DROPBEAR_KEYS, text.as_bytes(), 0o600)
        })
    }

    /// A system upgrade of OpenWrt keeps only the files listed in keep.d: limen's are its binary and /etc/limen.
    fn write_sysupgrade_keep(&self) -> Outcome<()> {
        let text = format!("{}\n/etc/limen/\n", self.binary);
        let dir = KEEP.rsplit_once('/').map_or("/", |(d, _)| d);
        if fs::stat(dir).map(|i| i.kind) != Some(fs::FileType::Directory)
            || fs::read_text(KEEP).as_deref() == Some(text.as_str())
        {
            return Ok(());
        }
        self.act(&format!("write {KEEP} (sysupgrade keeps limen)"), || fs::write_atomic(KEEP, text.as_bytes(), 0o644))
    }

    fn write_sudoers(&self, roles: &[Role]) -> Outcome<()> {
        let mut text = String::from(
            "# Written by `limen install`. Each limen user may run exactly its gate as root, nothing else.\n",
        );
        for role in roles {
            let user = user_of(*role);
            // SSH_CONNECTION only, for the audit log; never SSH_ORIGINAL_COMMAND, which limen ignores.
            text.push_str(&format!("Defaults:{user} env_keep += \"SSH_CONNECTION\"\n"));
            text.push_str(&format!("{user} ALL=(root) NOPASSWD: {} gate --role {}\n", self.binary, role.wire()));
        }
        if fs::read_following(SUDOERS).as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.act(&format!("write {SUDOERS} (checked with visudo first)"), || {
            // A name with a dot: sudo skips it, so a half-written check file is never in force.
            let check = "/etc/sudoers.d/limen.check";
            fs::write_atomic(check, text.as_bytes(), 0o440)?;
            let visudo = proc::which("visudo").ok_or("visudo is not installed")?;
            let r = proc::run(&[visudo, "-cf".into(), check.into()], proc::Run::default())?;
            fs::remove(check);
            if r.exit_code != 0 {
                return Err(format!("visudo rejected the sudoers file: {}{}", r.err().trim(), r.out().trim()));
            }
            fs::write_following(SUDOERS, text.as_bytes(), 0o440)?;
            fs::chown(SUDOERS, 0, 0)
        })
    }

    fn ensure_directories(&self, repo: Option<&RepoOptions>) -> Outcome<()> {
        for dir in ["/etc/limen", "/etc/limen/checks.d", "/etc/limen/actions.d", "/etc/limen/setup.d"] {
            if fs::stat(dir).map(|i| i.kind) != Some(fs::FileType::Directory) {
                self.act(&format!("create {dir}"), || fs::mkdirs(dir, 0o755))?;
            }
        }
        // Written into the operator's text, comments and all.
        let with_repo = |text: &str, r: &RepoOptions| {
            node_config::with_repo(text, &r.url, &r.branch, &r.path).map_err(|e| format!("--repo: {e}"))
        };
        match (fs::read_following(node_config::PATH), repo) {
            (None, _) => {
                self.act(&format!("write {} (nothing readable until you list it)", node_config::PATH), || {
                    let text = match repo {
                        Some(r) => with_repo(CONFIG_TEMPLATE, r)?,
                        None => format!("{CONFIG_TEMPLATE}\n{REPO_TEMPLATE}"),
                    };
                    fs::write_following(node_config::PATH, text.as_bytes(), 0o644)
                })?;
            }
            (Some(existing), Some(r)) => {
                let current = NodeConfig::parse(&existing).ok().and_then(|c| c.repo);
                match current {
                    None if Regex::new(r"(?m)^\s*\[repo]").unwrap().is_match(&existing) => {
                        self.warn(&format!(
                            "{} has a [repo] section limen can't read; left as it is",
                            node_config::PATH
                        ));
                    }
                    None => {
                        self.act(&format!("add [repo] to {}", node_config::PATH), || {
                            fs::write_following(node_config::PATH, with_repo(&existing, r)?.as_bytes(), 0o644)
                        })?;
                    }
                    Some(c) if c.url != r.url || c.branch != r.branch || c.path != r.path.trim_matches('/') => {
                        self.warn(&format!(
                            "{} already has another [repo] ({}); left as it is",
                            node_config::PATH,
                            c.display_url()
                        ));
                    }
                    Some(_) => {}
                }
            }
            (Some(_), None) => {}
        }
        for dir in ["/var/log/limen", "/var/log/limen/runs"] {
            if fs::stat(dir).map(|i| i.kind) != Some(fs::FileType::Directory) {
                self.act(&format!("create {dir} (root only)"), || fs::mkdirs(dir, 0o700))?;
            }
        }
        Ok(())
    }

    /// A forced command only holds a key-based login. With root's password empty —as OpenWrt ships— dropbear lets
    /// anyone who reaches it in without a key, and limen's limits mean nothing: say it.
    fn check_dropbear(&self) {
        if shadow_password("root").as_deref() == Some("") {
            self.warn("root has no password: dropbear lets anyone in without a key. Set one (passwd), or turn password logins off");
        }
        let Some(uci) = proc::which("uci") else { return };
        let password_auth = proc::run(
            &[uci, "-q".into(), "get".into(), "dropbear.@dropbear[0].PasswordAuth".into()],
            proc::Run::default(),
        )
        .map(|r| r.out().trim().to_string())
        .unwrap_or_default();
        if password_auth != "off" && password_auth != "0" {
            self.note(
                "dropbear accepts passwords; with keys only, nothing but limen's keys and yours gets in: \
                 uci set dropbear.@dropbear[0].PasswordAuth=off; uci set dropbear.@dropbear[0].RootPasswordAuth=off; \
                 uci commit dropbear; service dropbear restart",
            );
        }
    }

    /// `AllowUsers`/`AllowGroups` that would keep the new users out are the classic silent failure: say it.
    fn check_sshd(&self, users: &[String]) {
        let Some(sshd) = proc::which("sshd") else {
            self.note("sshd not found; limen needs it to be reachable");
            return;
        };
        let Ok(r) =
            proc::run(&[sshd, "-T".into()], proc::Run { timeout: Duration::from_secs(10), ..Default::default() })
        else {
            return;
        };
        if r.exit_code != 0 {
            return;
        }
        let out = r.out();
        let settings: Vec<(&str, &str)> = out.lines().filter_map(|l| l.split_once(' ')).collect();
        let values = |key: &str| -> Vec<&str> {
            settings.iter().filter(|(k, _)| *k == key).flat_map(|(_, v)| v.split(' ')).collect()
        };
        let (allow_users, allow_groups) = (values("allowusers"), values("allowgroups"));
        if !allow_users.is_empty() || !allow_groups.is_empty() {
            let missing: Vec<&str> = users
                .iter()
                .map(String::as_str)
                .filter(|u| !allow_users.contains(u) && !allow_groups.contains(u))
                .collect();
            if !missing.is_empty() {
                self.warn(&format!(
                    "sshd has AllowUsers/AllowGroups without {}: they can't log in",
                    missing.join(", ")
                ));
            }
        }
        if settings.iter().any(|(k, v)| *k == "pubkeyauthentication" && *v == "no") {
            self.warn("sshd has PubkeyAuthentication no");
        }
    }

    pub fn host_key(&self) -> Option<String> {
        let first_two = |s: &str| s.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        if self.openwrt {
            let dropbearkey = proc::which("dropbearkey")?;
            let r = proc::run(
                &[dropbearkey, "-y".into(), "-f".into(), "/etc/dropbear/dropbear_ed25519_host_key".into()],
                proc::Run::default(),
            )
            .ok()?;
            return r.out().lines().find(|l| l.starts_with("ssh-ed25519 ")).map(first_two);
        }
        fs::read_text("/etc/ssh/ssh_host_ed25519_key.pub").map(|k| first_two(k.trim()))
    }

    fn require_root(&self) -> Outcome<()> {
        if !self.dry_run && sys::euid() != 0 {
            return Err("run it as root (or with --dry-run to see what it would do)".into());
        }
        Ok(())
    }

    fn act(&self, what: &str, block: impl FnOnce() -> Outcome<()>) -> Outcome<()> {
        if self.dry_run {
            say(&format!("would {what}"));
            Ok(())
        } else {
            say(what);
            block()
        }
    }

    fn note(&self, text: &str) {
        say(&format!("  note: {text}"));
    }

    fn warn(&self, text: &str) {
        self.warnings.set(self.warnings.get() + 1);
        say(&format!("  WARNING: {text}"));
    }
}

fn say(text: &str) {
    sys::out(&format!("{text}\n"));
}

fn repo_config(options: &RepoOptions) -> Outcome<RepoConfig> {
    let text =
        node_config::with_repo("", &options.url, &options.branch, &options.path).map_err(|e| format!("--repo: {e}"))?;
    NodeConfig::parse(&text).map(|c| c.repo.expect("the section is there")).map_err(|e| format!("--repo: {e}"))
}

fn validate_key(option: &str, key: &str) -> Outcome<()> {
    if Regex::new(PUBLIC_KEY).unwrap().is_match(key.trim()) {
        Ok(())
    } else {
        Err(format!("{option} is not an SSH public key ('ssh-ed25519 AAAA… comment')"))
    }
}

fn shadow_password(user: &str) -> Option<String> {
    fs::read_text("/etc/shadow")?
        .lines()
        .find(|l| l.starts_with(&format!("{user}:")))
        .and_then(|l| l.split(':').nth(1).map(String::from))
}

fn exec(argv: &[&str]) -> Outcome<()> {
    let path = proc::which(argv[0]).ok_or(format!("{} is not installed", argv[0]))?;
    let full: Vec<String> = std::iter::once(path).chain(argv[1..].iter().map(|s| s.to_string())).collect();
    let r = proc::run(&full, proc::Run { timeout: Duration::from_secs(60), ..Default::default() })?;
    if r.exit_code != 0 {
        return Err(format!("{}: {}", argv.join(" "), r.err().trim()));
    }
    Ok(())
}

const CONFIG_TEMPLATE: &str = r#"# limen on this node (docs/spec.md §7.1). Read by every request; no restart needed.

[files]
# Nothing is readable until it is listed here. Whatever is readable ends up in the context of the model
# the hub talks to, so list what helps diagnose and nothing that holds a secret.
allow = [
  # "/etc/nginx/**",
  # "/etc/systemd/system/*.service",
  # "/opt/stacks/*/compose.yaml",
  # "/var/log/nginx/*.log",
]
deny = [
  "**/*.env",
]
# max_bytes = 262144

[logs]
# max_lines = 2000
# scan_lines = 100000

[redact]
# Added to the built-in patterns. A group named `secret` limits what is replaced.
patterns = []
"#;

const REPO_TEMPLATE: &str = r#"# The repository this node takes its scripts, stacks and node.toml from (`limen install --repo` fills it).
# [repo]
# url = "https://github.com/<owner>/<repo>.git"
# branch = "main"
# path = "nodes/<this node>"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_configuration_that_allows_nothing() {
        let c = NodeConfig::parse(&format!("{CONFIG_TEMPLATE}\n{REPO_TEMPLATE}")).unwrap();
        assert!(c.allow.is_empty());
        assert_eq!(c.deny, ["**/*.env"]);
        assert!(c.repo.is_none());
    }

    #[test]
    fn keys_and_cidrs() {
        assert!(validate_key("k", "ssh-ed25519 AAAAC3Nza root@nas").is_ok());
        assert!(validate_key("k", "ssh-ed25519 AAAA\"; rm -rf /").is_err());
        assert!(validate_key("k", "command=\"x\" ssh-ed25519 AAAA").is_err());
        let cidrs = Regex::new(CIDRS).unwrap();
        assert!(cidrs.is_match("100.64.0.0/10,192.168.1.*"));
        assert!(!cidrs.is_match("10.0.0.0/8\" ssh-ed25519"));
    }
}
