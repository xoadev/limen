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
use std::sync::LazyLock;
use std::time::Duration;

pub const SUDOERS: &str = "/etc/sudoers.d/limen";
pub const DROPBEAR_KEYS: &str = "/etc/dropbear/authorized_keys";
pub const SYSUPGRADE_KEEP: &str = "/lib/upgrade/keep.d/limen";
/// What marks limen's lines in a shared authorized_keys: its forced command.
const LIMEN_LINE: &str = "limen gate --role";
const ROOT_KEY_FILES: [&str; 2] = ["/root/.ssh/authorized_keys", "/root/.ssh/authorized_keys2"];

/// One public key, with no options and a comment without quotes or line breaks: it is written after a forced command.
static PUBLIC_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com|sk-ecdsa-sha2-nistp256@openssh\.com) [A-Za-z0-9+/]+=*( [^\r\n"]*)?$"#)
        .expect("a valid pattern")
});
/// What sshd's `from=` takes —addresses, CIDRs, wildcards, negations— and nothing that could close the option.
static FROM_ADDRESSES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9A-Fa-f.:/*?,!]+$").expect("a valid pattern"));
static REPO_SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*\[repo]").expect("a valid pattern"));

pub type Outcome<T> = Result<T, String>;

/// The repository a node follows, its folder settled.
#[derive(Debug, Clone)]
pub struct RepoOptions {
    pub url: String,
    pub branch: String,
    pub path: String,
}

/// `--repo`, `--branch` and `--path` as given: without `--path`, the folder is the node's name, which a join only
/// learns from the hub.
#[derive(Debug, Clone)]
pub struct RepoChoice {
    pub url: String,
    pub branch: String,
    pub path: Option<String>,
}

impl RepoChoice {
    pub fn for_node(&self, node: &str) -> RepoOptions {
        RepoOptions {
            url: self.url.clone(),
            branch: self.branch.clone(),
            path: self.path.clone().unwrap_or_else(|| format!("nodes/{node}")),
        }
    }
}

/// What `install` and `join` set up on a machine besides the hub's key.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub deploy_key: Option<String>,
    pub from: Option<String>,
    pub repo: Option<RepoChoice>,
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
        sys::umask_022();
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

    /// Sets this machine up with the hub's [read_key] and [setup]; [node] is its name, the repository's default folder.
    /// Without a read key there is no hub yet: the machine can follow its repository, and a join adds the hub later.
    pub fn install(&self, read_key: Option<&str>, setup: &Setup, node: &str, announce: bool) -> Outcome<i32> {
        let (deploy_key, from) = (setup.deploy_key.as_deref(), setup.from.as_deref());
        let repo = setup.repo.as_ref().map(|choice| choice.for_node(node));
        let repo = repo.as_ref();
        self.check_arguments(read_key, deploy_key, from, repo.is_some())?;
        let repo_config = repo.map(repo_config).transpose()?;
        self.require_root()?;
        let keys: Vec<(Role, &str)> = read_key
            .map(|key| (Role::Read, key))
            .into_iter()
            .chain(deploy_key.map(|key| (Role::Deploy, key)))
            .collect();
        self.refuse_keys_opened_elsewhere(&keys)?;
        if let Some(repo_config) = &repo_config {
            self.check_repo(repo_config)?;
        }
        self.install_binary()?;
        if !keys.is_empty() {
            if self.openwrt {
                self.wire_dropbear(&keys)?;
            } else {
                self.wire_openssh(&keys, from)?;
            }
        }
        self.ensure_directories(repo)?;
        if let Some(repo_config) = &repo_config {
            self.connect_repo(repo_config)?;
        }
        match (announce, read_key) {
            (false, _) => {}
            (true, Some(_)) => self.announce_next_steps(),
            (true, None) => self.announce_without_hub(),
        }
        Ok(0)
    }

    pub fn uninstall(&self, purge: bool) -> Outcome<i32> {
        self.require_root()?;
        let repo = Node::load(node_config::PATH).ok().and_then(|node| node.config.repo);
        if self.openwrt {
            self.unwire_dropbear()?;
        } else {
            self.unwire_openssh()?;
        }
        if purge {
            self.purge(repo.as_ref())?;
        } else {
            self.note("kept /etc/limen, the logs and the repository checkout (--purge removes them)");
        }
        self.remove_file(self.binary)?;
        self.conclude("limen is uninstalled.");
        Ok(0)
    }

    /// `limen token`: asks for the repository token again, checks it and saves it. For when it expires.
    pub fn token(&self) -> Outcome<i32> {
        self.require_root()?;
        let node = Node::load(node_config::PATH).map_err(|failure| failure.message)?;
        let repo = node.config.repo.ok_or(format!("no [repo] in {}", node_config::PATH))?;
        self.ask_token(&repo, true)?;
        Ok(0)
    }

    /// This machine's ed25519 host key without its comment: what the hub pins.
    pub fn host_key(&self) -> Option<String> {
        let type_and_blob = |key: &str| key.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        if self.openwrt {
            let dropbearkey = proc::which("dropbearkey")?;
            let result = proc::run(
                &[dropbearkey, "-y".into(), "-f".into(), "/etc/dropbear/dropbear_ed25519_host_key".into()],
                proc::Run::default(),
            )
            .ok()?;
            return result.out().lines().find(|line| line.starts_with("ssh-ed25519 ")).map(type_and_blob);
        }
        fs::read_text("/etc/ssh/ssh_host_ed25519_key.pub").map(|key| type_and_blob(key.trim()))
    }

    /// The arguments on their own, before anything on the machine is looked at.
    fn check_arguments(
        &self,
        read_key: Option<&str>,
        deploy_key: Option<&str>,
        from: Option<&str>,
        follows_repo: bool,
    ) -> Outcome<()> {
        if read_key.is_none() && deploy_key.is_none() && !follows_repo {
            return Err("nothing to install: give --read-key (the hub's key), --repo, or both".into());
        }
        if let Some(read_key) = read_key {
            validate_key("--read-key", read_key)?;
        }
        if let Some(deploy_key) = deploy_key {
            validate_key("--deploy-key", deploy_key)?;
        }
        if let Some(from) = from {
            if read_key.is_none() && deploy_key.is_none() {
                return Err("--from limits where the keys connect from; give --read-key or --deploy-key".into());
            }
            if !FROM_ADDRESSES.is_match(from) {
                return Err(format!("--from '{from}' is not a list of addresses or CIDRs"));
            }
            if self.openwrt {
                return Err(
                    "dropbear has no from= option; limit port 22 with the firewall and run it without --from".into()
                );
            }
        }
        let same_key =
            matches!((read_key, deploy_key), (Some(read_key), Some(deploy_key)) if key_blob(read_key) == key_blob(deploy_key));
        if same_key {
            return Err("--read-key and --deploy-key are the same key: the hub would hold the deploy role too".into());
        }
        Ok(())
    }

    /// A key that also opens root, or the deploy user, would take whoever holds it past the gate.
    fn refuse_keys_opened_elsewhere(&self, keys: &[(Role, &str)]) -> Outcome<()> {
        for (role, key) in keys {
            if let Some(place) = self.opened_elsewhere(*role, key_blob(key), installs_deploy(keys)) {
                return Err(format!(
                    "the {} key already opens {place} without limen's limits; remove it there, or give limen a key of \
                     its own",
                    role.wire()
                ));
            }
        }
        Ok(())
    }

    /// Where [blob] already opens this machine beyond limen's gate: root's own keys, dropbear's keys that aren't
    /// limen's, or —when the deploy role isn't being installed now— the deploy user's, for the read key.
    fn opened_elsewhere(&self, role: Role, blob: &str, deploy_installed: bool) -> Option<String> {
        // Each file, and whether limen's own lines in it are to be left out.
        let mut key_files: Vec<(String, bool)> = ROOT_KEY_FILES.map(|path| (path.to_string(), false)).to_vec();
        if self.openwrt {
            key_files.push((DROPBEAR_KEYS.into(), true));
        } else if role == Role::Read && !deploy_installed {
            if let Some(deploy) = fs::account(&user_of(Role::Deploy)) {
                key_files.push((format!("{}/.ssh/authorized_keys", deploy.home), false));
            }
        }
        key_files
            .into_iter()
            .find(|(path, without_limen_lines)| lists_key(path, blob, *without_limen_lines))
            .map(|(path, _)| path)
    }

    fn check_repo(&self, repo: &RepoConfig) -> Outcome<()> {
        refuse_other_repo(repo)?;
        if proc::which("git").is_none() {
            let how = if self.openwrt { "the git-http package" } else { "apt install git" };
            return Err(format!("--repo needs git on this node ({how})"));
        }
        // Access to the repository first, token included: a repository that can't be read stops the install before
        // anything on the machine has changed.
        if !self.dry_run {
            self.ask_token(repo, false)?;
        }
        Ok(())
    }

    fn install_binary(&self) -> Outcome<()> {
        let running = fs::real_path("/proc/self/exe").ok_or("cannot find this binary")?;
        if running == self.binary {
            return Ok(());
        }
        let bytes = fs::read(&running, usize::MAX).ok_or(format!("cannot read {running}"))?;
        let installed = fs::real_path(self.binary).and_then(|path| fs::read(&path, usize::MAX));
        if installed.as_ref() == Some(&bytes) {
            return Ok(());
        }
        self.act(&format!("install this binary as {}", self.binary), || {
            fs::mkdirs(fs::parent(self.binary), 0o755)?;
            fs::write_atomic(self.binary, &bytes, 0o755)?;
            fs::chown(self.binary, 0, 0)
        })?;
        // sudo runs it as root for anyone holding a key: whoever can replace it, or its directory, is root.
        if let Some(why) = limen_core::trust::problem(&fs::chain(self.binary), 0).filter(|_| !self.dry_run) {
            self.warn(&format!("{why}: whoever can write there runs anything as root through limen's sudo rule"));
        }
        Ok(())
    }

    fn wire_dropbear(&self, keys: &[(Role, &str)]) -> Outcome<()> {
        self.write_dropbear_keys(keys)?;
        self.write_sysupgrade_keep()?;
        self.check_dropbear();
        Ok(())
    }

    fn wire_openssh(&self, keys: &[(Role, &str)], from: Option<&str>) -> Outcome<()> {
        for (role, key) in keys {
            let user = user_of(*role);
            self.ensure_user(&user)?;
            self.write_authorized_keys(&user, *role, key, from)?;
        }
        if !installs_deploy(keys) && fs::account(&user_of(Role::Deploy)).is_some() {
            self.note(&format!(
                "{} exists from an earlier install; left as it is (uninstall removes it)",
                user_of(Role::Deploy)
            ));
        }
        let roles: Vec<Role> = keys.iter().map(|(role, _)| *role).collect();
        self.write_sudoers(&roles)?;
        self.check_sshd(&roles.iter().map(|role| user_of(*role)).collect::<Vec<_>>());
        Ok(())
    }

    fn unwire_dropbear(&self) -> Outcome<()> {
        self.remove_dropbear_keys()?;
        self.remove_file(SYSUPGRADE_KEEP)
    }

    fn unwire_openssh(&self) -> Outcome<()> {
        for role in [Role::Read, Role::Deploy] {
            self.remove_user(&user_of(role))?;
        }
        self.remove_file(SUDOERS)
    }

    fn ensure_user(&self, user: &str) -> Outcome<()> {
        let existing = fs::account(user);
        if existing.is_none() {
            let home = default_home(user);
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
        // Its home is root's, as its .ssh is: the account can't swap what limen writes there for a link.
        let home = home_of(user);
        if fs::lstat(&home).is_some_and(|info| info.uid != 0 || info.mode & 0o022 != 0) {
            self.act(&format!("make {home} root's (mode 0755)"), || {
                fs::chown(&home, 0, 0)?;
                fs::chmod(&home, 0o755)
            })?;
        }
        // `/bin/sh` and not `nologin`: sshd runs the forced command through the login shell, and with `nologin`
        // nothing runs. `*` and not `!`: a locked password makes sshd refuse even key logins when PAM is off.
        if existing.as_ref().is_none_or(|account| account.shell != "/bin/sh")
            || shadow_password(user).as_deref() != Some("*")
        {
            self.act(&format!("set {user}'s shell to /bin/sh and its password to none (key login only)"), || {
                exec(&["usermod", "--shell", "/bin/sh", "--password", "*", user])
            })?;
        }
        Ok(())
    }

    fn remove_user(&self, user: &str) -> Outcome<()> {
        let Some(account) = fs::account(user) else { return Ok(()) };
        // Its home is root's (see ensure_user), which userdel won't remove: limen removes the one it made.
        let home = default_home(user);
        self.act(&format!("remove user {user} and its home"), || {
            exec(&["userdel", user])?;
            if account.home == home && fs::exists(&home) {
                exec(&["rm", "-rf", "--", &home])?;
            }
            Ok(())
        })
    }

    fn write_authorized_keys(&self, user: &str, role: Role, key: &str, from: Option<&str>) -> Outcome<()> {
        let home = home_of(user);
        let from = from.map(|addresses| format!(",from=\"{addresses}\"")).unwrap_or_default();
        let line = format!("restrict{from},command=\"sudo -n {}\" {}\n", self.gate_command(role), key.trim());
        let path = format!("{home}/.ssh/authorized_keys");
        if fs::read_following(&path).as_deref() == Some(line.as_str()) {
            return Ok(());
        }
        // Owned by root: the account itself can't change which key opens it or what that key runs.
        self.act(&format!("write {path} ({} role, forced command)", role.wire()), || {
            let ssh_dir = format!("{home}/.ssh");
            fs::mkdirs(&ssh_dir, 0o755)?;
            fs::chown(&ssh_dir, 0, 0)?;
            fs::chmod(&ssh_dir, 0o755)?;
            fs::write_following(&path, line.as_bytes(), 0o644)?;
            fs::chown(&path, 0, 0)
        })
    }

    /// Root's `authorized_keys` of dropbear, shared with whoever administers the router: only limen's own lines
    /// (those with its forced command) are replaced; every other key stays as it was.
    fn write_dropbear_keys(&self, keys: &[(Role, &str)]) -> Outcome<()> {
        let current = fs::read_following(DROPBEAR_KEYS);
        let mut lines: Vec<String> = other_keys(current.as_deref().unwrap_or("")).map(String::from).collect();
        lines.extend(keys.iter().map(|(role, key)| {
            format!(
                "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty,command=\"{}\" {}",
                self.gate_command(*role),
                key.trim()
            )
        }));
        let text = format!("{}\n", lines.join("\n"));
        if current.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        let roles: Vec<&str> = keys.iter().map(|(role, _)| role.wire()).collect();
        self.act(
            &format!("write limen's keys in {DROPBEAR_KEYS} ({}, forced commands; other keys kept)", roles.join(", ")),
            || {
                fs::mkdirs(fs::parent(DROPBEAR_KEYS), 0o700)?;
                fs::write_following(DROPBEAR_KEYS, text.as_bytes(), 0o600)
            },
        )
    }

    fn remove_dropbear_keys(&self) -> Outcome<()> {
        let Some(current) = fs::read_following(DROPBEAR_KEYS) else { return Ok(()) };
        if !current.lines().any(is_limen_line) {
            return Ok(());
        }
        self.act(&format!("remove limen's keys from {DROPBEAR_KEYS} (other keys kept)"), || {
            let kept: Vec<&str> = other_keys(&current).collect();
            let text = if kept.is_empty() { String::new() } else { format!("{}\n", kept.join("\n")) };
            fs::write_following(DROPBEAR_KEYS, text.as_bytes(), 0o600)
        })
    }

    /// A system upgrade of OpenWrt keeps only the files listed in keep.d: limen's are its binary and /etc/limen.
    fn write_sysupgrade_keep(&self) -> Outcome<()> {
        let text = format!("{}\n/etc/limen/\n", self.binary);
        if !fs::is_directory(fs::parent(SYSUPGRADE_KEEP))
            || fs::read_text(SYSUPGRADE_KEEP).as_deref() == Some(text.as_str())
        {
            return Ok(());
        }
        self.act(&format!("write {SYSUPGRADE_KEEP} (sysupgrade keeps limen)"), || {
            fs::write_atomic(SYSUPGRADE_KEEP, text.as_bytes(), 0o644)
        })
    }

    fn write_sudoers(&self, roles: &[Role]) -> Outcome<()> {
        let mut text = String::from(
            "# Written by `limen install`. Each limen user may run exactly its gate as root, nothing else.\n",
        );
        for role in roles {
            let user = user_of(*role);
            // SSH_CONNECTION only, for the audit log; never SSH_ORIGINAL_COMMAND, which limen ignores.
            text.push_str(&format!("Defaults:{user} env_keep += \"SSH_CONNECTION\"\n"));
            text.push_str(&format!("{user} ALL=(root) NOPASSWD: {}\n", self.gate_command(*role)));
        }
        if fs::read_following(SUDOERS).as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.act(&format!("write {SUDOERS} (checked with visudo first)"), || {
            check_sudoers(&text)?;
            fs::write_following(SUDOERS, text.as_bytes(), 0o440)?;
            fs::chown(SUDOERS, 0, 0)
        })
    }

    fn ensure_directories(&self, repo: Option<&RepoOptions>) -> Outcome<()> {
        for dir in ["/etc/limen", "/etc/limen/checks.d", "/etc/limen/actions.d", "/etc/limen/setup.d"] {
            self.ensure_directory(dir, 0o755, "")?;
        }
        self.write_config(repo)?;
        for dir in ["/var/log/limen", "/var/log/limen/runs"] {
            self.ensure_directory(dir, 0o700, " (root only)")?;
        }
        Ok(())
    }

    fn ensure_directory(&self, dir: &str, mode: u32, detail: &str) -> Outcome<()> {
        if fs::is_directory(dir) {
            return Ok(());
        }
        self.act(&format!("create {dir}{detail}"), || fs::mkdirs(dir, mode))
    }

    /// limen.toml: the template when there is none, or `[repo]` added to the operator's, comments and all.
    fn write_config(&self, repo: Option<&RepoOptions>) -> Outcome<()> {
        match (fs::read_following(node_config::PATH), repo) {
            (None, _) => self.act(&format!("write {} (nothing readable until you list it)", node_config::PATH), || {
                let text = match repo {
                    Some(repo) => with_repo(CONFIG_TEMPLATE, repo)?,
                    None => format!("{CONFIG_TEMPLATE}\n{REPO_TEMPLATE}"),
                };
                fs::write_following(node_config::PATH, text.as_bytes(), 0o644)
            }),
            (Some(existing), Some(repo)) => self.add_repo_section(&existing, repo),
            (Some(_), None) => Ok(()),
        }
    }

    fn add_repo_section(&self, existing: &str, repo: &RepoOptions) -> Outcome<()> {
        // Another [repo] stopped the install before it changed anything.
        if NodeConfig::parse(existing).ok().and_then(|config| config.repo).is_some() {
            return Ok(());
        }
        if REPO_SECTION.is_match(existing) {
            self.warn(&format!("{} has a [repo] section limen can't read; left as it is", node_config::PATH));
            return Ok(());
        }
        self.act(&format!("add [repo] to {}", node_config::PATH), || {
            fs::write_following(node_config::PATH, with_repo(existing, repo)?.as_bytes(), 0o644)
        })
    }

    /// The repository: a token when it needs one, and the first checkout.
    fn connect_repo(&self, repo: &RepoConfig) -> Outcome<()> {
        if self.dry_run {
            sys::say(&format!(
                "would check access to {}, ask for a token if it needs one, and sync it to {}",
                repo.display_url(),
                repo.dir
            ));
            return Ok(());
        }
        sys::say(&format!("sync {} {} into {}", repo.display_url(), repo.branch, repo.dir));
        let node = Node::load(node_config::PATH).map_err(|failure| failure.message)?;
        let (_, commit) = repo::sync(&node, repo).map_err(|failure| format!("sync: {}", failure.message))?;
        self.note(&format!("at {}; `limen apply` runs its setup scripts and stacks", repo::short_hash(&commit)));
        Ok(())
    }

    /// A token that reads the repository, asked for, checked and saved. Unless [force]d, none is asked for while the
    /// repository reads without one, or with the one saved.
    fn ask_token(&self, repo: &RepoConfig, force: bool) -> Outcome<()> {
        if !force && self.readable_already(repo)? {
            return Ok(());
        }
        if !repo.url.starts_with("https://") {
            return Err(format!("{} is not readable, and a token only works over https://", repo.display_url()));
        }
        explain_token(repo);
        for _ in 0..3 {
            let token = prompt_token()?;
            match repo::access(repo, Some(&token)) {
                repo::Access::Failed(message) => return Err(cannot_reach(repo, &message)),
                repo::Access::Readable => return save_token(repo, &token),
                repo::Access::NeedsToken => sys::say(&format!("that token can't read {}", repo.display_url())),
            }
        }
        Err(format!("no token that reads {}", repo.display_url()))
    }

    /// Whether the repository reads without a new token: with none, or with the one saved.
    fn readable_already(&self, repo: &RepoConfig) -> Outcome<bool> {
        match repo::access(repo, None) {
            repo::Access::Readable => {
                self.note(&format!("{} is readable without a token", repo.display_url()));
                return Ok(true);
            }
            repo::Access::Failed(message) => return Err(cannot_reach(repo, &message)),
            repo::Access::NeedsToken => {}
        }
        let saved_token_reads =
            repo::token(repo).is_some_and(|saved| matches!(repo::access(repo, Some(&saved)), repo::Access::Readable));
        if saved_token_reads {
            self.note(&format!("the saved token reads {}", repo.display_url()));
        }
        Ok(saved_token_reads)
    }

    /// `--purge`: the configuration, the logs, and the repository checkout if sync made it.
    fn purge(&self, repo: Option<&RepoConfig>) -> Outcome<()> {
        let mut paths = vec!["/etc/limen".to_string(), "/var/log/limen".to_string()];
        if let Some(repo) = repo {
            // repo.dir is removed only if it is what sync made of it: a checkout.
            if repo::is_checkout(repo) {
                paths.push(repo.dir.clone());
            } else if fs::exists(&repo.dir) {
                self.note(&format!("kept {}: it is not a git checkout", repo.dir));
            }
            paths.push(repo.sync_record());
        }
        for path in paths.iter().filter(|path| fs::exists(path)) {
            self.act(&format!("remove {path}"), || exec(&["rm", "-rf", "--", path]))?;
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
        .map(|result| result.out().trim().to_string())
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
        let Some(effective) = effective_sshd_config(sshd) else { return };
        let settings: Vec<(&str, &str)> = effective.lines().filter_map(|line| line.split_once(' ')).collect();
        let shut_out = users_shut_out(users, &settings);
        if !shut_out.is_empty() {
            self.warn(&format!("sshd has AllowUsers/AllowGroups without {}: they can't log in", shut_out.join(", ")));
        }
        if settings.iter().any(|(key, value)| *key == "pubkeyauthentication" && *value == "no") {
            self.warn("sshd has PubkeyAuthentication no");
        }
    }

    /// What every key is held to: its role's gate, and nothing else.
    fn gate_command(&self, role: Role) -> String {
        format!("{} gate --role {}", self.binary, role.wire())
    }

    fn announce_next_steps(&self) {
        sys::say("");
        self.conclude("limen is installed.");
        sys::say(&format!(
            "Next: list what may be read in {} ([files].allow is empty), then add this node to the hub:",
            node_config::PATH
        ));
        sys::say(&format!("  [nodes.{}]", node_name()));
        sys::say("  host = \"<address>\"");
        if self.openwrt {
            sys::say("  user = \"root\"");
        }
        let host_key = self.host_key().unwrap_or("<ssh-keyscan -t ed25519 this-host, checked out of band>".into());
        sys::say(&format!("  host_key = \"{host_key}\""));
        if self.warnings.get() > 0 {
            sys::say(&format!("{} warning(s) above.", self.warnings.get()));
        }
    }

    /// No hub yet: what the machine can already do, and how a hub joins it later.
    fn announce_without_hub(&self) {
        sys::say("");
        self.conclude("limen is installed, with no hub yet.");
        sys::say("`sudo limen apply` brings this machine to its folder of the repository. A hub joins it later:");
        sys::say("  on the hub, `limen invite <name>`; here, as root, the line that prints");
        if self.warnings.get() > 0 {
            sys::say(&format!("{} warning(s) above.", self.warnings.get()));
        }
    }

    fn require_root(&self) -> Outcome<()> {
        if !self.dry_run && sys::euid() != 0 {
            return Err("run it as root (or with --dry-run to see what it would do)".into());
        }
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Outcome<()> {
        if !fs::exists(path) {
            return Ok(());
        }
        self.act(&format!("remove {path}"), || {
            fs::remove(path);
            Ok(())
        })
    }

    fn act(&self, what: &str, block: impl FnOnce() -> Outcome<()>) -> Outcome<()> {
        if self.dry_run {
            sys::say(&format!("would {what}"));
            Ok(())
        } else {
            sys::say(what);
            block()
        }
    }

    fn conclude(&self, done: &str) {
        sys::say(if self.dry_run { "Dry run: nothing was changed." } else { done });
    }

    fn note(&self, text: &str) {
        sys::say(&format!("  note: {text}"));
    }

    fn warn(&self, text: &str) {
        self.warnings.set(self.warnings.get() + 1);
        sys::say(&format!("  WARNING: {text}"));
    }
}

fn repo_config(options: &RepoOptions) -> Outcome<RepoConfig> {
    NodeConfig::parse(&with_repo("", options)?)
        .map(|config| config.repo.expect("the section is there"))
        .map_err(|problem| format!("--repo: {problem}"))
}

/// [text] with `[repo]` set from the options, through toml_edit: the rest of the text stays as it was.
fn with_repo(text: &str, repo: &RepoOptions) -> Outcome<String> {
    node_config::with_repo(text, &repo.url, &repo.branch, &repo.path).map_err(|problem| format!("--repo: {problem}"))
}

/// Another repository than the one configured would be synced now and then left behind: the configuration wins, and
/// changing it is the operator's call.
fn refuse_other_repo(repo: &RepoConfig) -> Outcome<()> {
    let configured = fs::read_following(node_config::PATH).and_then(|text| NodeConfig::parse(&text).ok()?.repo);
    let Some(configured) = configured else { return Ok(()) };
    if configured.url == repo.url && configured.branch == repo.branch && configured.path == repo.path {
        return Ok(());
    }
    Err(format!(
        "{} already follows {} ({}, {}); change it there, or uninstall first",
        node_config::PATH,
        configured.display_url(),
        configured.branch,
        if configured.path.is_empty() { "/" } else { &configured.path }
    ))
}

fn cannot_reach(repo: &RepoConfig, message: &str) -> String {
    format!("cannot reach {}: {message}", repo.display_url())
}

fn explain_token(repo: &RepoConfig) {
    sys::say("");
    sys::say(&format!("{} needs a token that can read it.", repo.display_url()));
    let Some((owner, name)) = repo.github() else { return };
    sys::say("Create one here (fine-grained, read-only contents, no expiry):");
    sys::say(&format!("  {}", github::token_url(&owner, &name, &sys::hostname())));
    sys::say(&format!(
        "In the form, check that the resource owner is {owner} (select it again if in doubt) and, under"
    ));
    sys::say(&format!("Repository access, choose \"Only select repositories\" and {name}."));
}

/// One token from the operator: typed unseen on a terminal, or a line from a pipe.
fn prompt_token() -> Outcome<String> {
    sys::err("Token: ");
    let answer = if sys::stdin_is_terminal() { sys::read_secret() } else { sys::read_line() };
    answer.map(|token| token.trim().to_string()).filter(|token| !token.is_empty()).ok_or("no token given".into())
}

fn save_token(repo: &RepoConfig, token: &str) -> Outcome<()> {
    fs::mkdirs(fs::parent(&repo.token_file), 0o755)?;
    fs::write_atomic(&repo.token_file, format!("{token}\n").as_bytes(), 0o600)?;
    fs::chown(&repo.token_file, 0, 0)?;
    sys::say(&format!("token saved in {} (root only)", repo.token_file));
    Ok(())
}

fn validate_key(option: &str, key: &str) -> Outcome<()> {
    if PUBLIC_KEY.is_match(key.trim()) {
        Ok(())
    } else {
        Err(format!("{option} is not an SSH public key ('ssh-ed25519 AAAA… comment')"))
    }
}

/// The base64 of a public key: what identifies it, whatever its comment.
fn key_blob(key: &str) -> &str {
    key.split_whitespace().nth(1).unwrap_or("")
}

fn installs_deploy(keys: &[(Role, &str)]) -> bool {
    keys.iter().any(|(role, _)| *role == Role::Deploy)
}

fn is_limen_line(line: &str) -> bool {
    line.contains(LIMEN_LINE)
}

/// The keys of a shared authorized_keys that aren't limen's.
fn other_keys(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|line| !line.trim().is_empty() && !is_limen_line(line))
}

/// Whether the authorized_keys at [path] lists [blob], limen's own lines left out when asked.
fn lists_key(path: &str, blob: &str, without_limen_lines: bool) -> bool {
    fs::read_following(path).is_some_and(|text| {
        text.lines()
            .filter(|line| !(without_limen_lines && is_limen_line(line)))
            .any(|line| line.split_whitespace().any(|word| word == blob))
    })
}

/// visudo's verdict on [text], from a file sudo doesn't read.
fn check_sudoers(text: &str) -> Outcome<()> {
    // A name with a dot: sudo skips it, so a half-written check file is never in force.
    let check_file = "/etc/sudoers.d/limen.check";
    fs::write_atomic(check_file, text.as_bytes(), 0o440)?;
    let visudo = proc::which("visudo").ok_or("visudo is not installed")?;
    let result = proc::run(&[visudo, "-cf".into(), check_file.into()], proc::Run::default())?;
    fs::remove(check_file);
    if result.exit_code != 0 {
        return Err(format!("visudo rejected the sudoers file: {}{}", result.err().trim(), result.out().trim()));
    }
    Ok(())
}

/// `sshd -T`: the settings sshd runs with, one lowercase `key value` per line.
fn effective_sshd_config(sshd: String) -> Option<String> {
    let result =
        proc::run(&[sshd, "-T".into()], proc::Run { timeout: Duration::from_secs(10), ..Default::default() }).ok()?;
    (result.exit_code == 0).then(|| result.out())
}

/// The users that sshd's `AllowUsers`/`AllowGroups` leave out, if it has either. Each limen user has a group of its
/// own name (`useradd --user-group`), so either list may let it in.
fn users_shut_out<'a>(users: &'a [String], settings: &[(&str, &str)]) -> Vec<&'a str> {
    let values = |key: &str| -> Vec<&str> {
        settings.iter().filter(|(name, _)| *name == key).flat_map(|(_, value)| value.split(' ')).collect()
    };
    let (allow_users, allow_groups) = (values("allowusers"), values("allowgroups"));
    if allow_users.is_empty() && allow_groups.is_empty() {
        return vec![];
    }
    users
        .iter()
        .map(String::as_str)
        .filter(|user| !allow_users.contains(user) && !allow_groups.contains(user))
        .collect()
}

fn shadow_password(user: &str) -> Option<String> {
    fs::read_text("/etc/shadow")?
        .lines()
        .find(|line| line.starts_with(&format!("{user}:")))
        .and_then(|line| line.split(':').nth(1).map(String::from))
}

/// This machine's name as the hub would call it: the hostname in lowercase, letters, digits and dashes only.
fn node_name() -> String {
    sys::hostname()
        .to_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect()
}

/// Where `install` makes a limen user's home.
fn default_home(user: &str) -> String {
    format!("/var/lib/{user}")
}

fn home_of(user: &str) -> String {
    fs::account(user).map_or_else(|| default_home(user), |account| account.home)
}

/// A system program by name, with a minute to finish; what it says on stderr is the error.
fn exec(argv: &[&str]) -> Outcome<()> {
    let located = proc::located(argv).ok_or(format!("{} is not installed", argv[0]))?;
    let result = proc::run(&located, proc::Run { timeout: Duration::from_secs(60), ..Default::default() })?;
    if result.exit_code != 0 {
        return Err(format!("{}: {}", argv.join(" "), result.err().trim()));
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

[limits]
# max_response = 1048576
# concurrency = 8

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
        let config = NodeConfig::parse(&format!("{CONFIG_TEMPLATE}\n{REPO_TEMPLATE}")).unwrap();
        assert!(config.allow.is_empty());
        assert_eq!(config.deny, ["**/*.env"]);
        assert!(config.repo.is_none());
    }

    #[test]
    fn keys_and_cidrs() {
        assert!(validate_key("k", "ssh-ed25519 AAAAC3Nza root@nas").is_ok());
        assert!(validate_key("k", "ssh-ed25519 AAAA\"; rm -rf /").is_err());
        assert!(validate_key("k", "command=\"x\" ssh-ed25519 AAAA").is_err());
        assert!(FROM_ADDRESSES.is_match("100.64.0.0/10,192.168.1.*"));
        assert!(!FROM_ADDRESSES.is_match("10.0.0.0/8\" ssh-ed25519"));
    }
}
