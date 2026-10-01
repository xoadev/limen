//! `limen install` and `uninstall` (spec §3, §10): the wiring that makes the gate the only way in. Every step says
//! what it does; `--dry-run` says it without doing it, and running it twice changes nothing the second time.
//!
//! Two wirings. Debian and the like: OpenSSH, the system user `limen` and a sudo rule for exactly the gate. OpenWrt:
//! dropbear, which has neither sudo nor extra users, so the hub's key belongs to root, held to the gate by its forced
//! command.

use crate::os::{fs, proc, sys};
use limen_core::config::hub::NODE_USER;
use limen_core::config::node as node_config;
use regex::Regex;
use std::cell::Cell;
use std::sync::LazyLock;
use std::time::Duration;

pub const SUDOERS: &str = "/etc/sudoers.d/limen";
pub const DROPBEAR_KEYS: &str = "/etc/dropbear/authorized_keys";
pub const SYSUPGRADE_KEEP: &str = "/lib/upgrade/keep.d/limen";
/// What marks limen's line in a shared authorized_keys: its forced command.
const LIMEN_LINE: &str = "limen gate";
const ROOT_KEY_FILES: [&str; 2] = ["/root/.ssh/authorized_keys", "/root/.ssh/authorized_keys2"];

/// One public key, with no options and a comment without quotes or line breaks: it is written after a forced command.
static PUBLIC_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\.com|sk-ecdsa-sha2-nistp256@openssh\.com) [A-Za-z0-9+/]+=*( [^\r\n"]*)?$"#)
        .expect("a valid pattern")
});
/// What sshd's `from=` takes —addresses, CIDRs, wildcards, negations— and nothing that could close the option.
static FROM_ADDRESSES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9A-Fa-f.:/*?,!]+$").expect("a valid pattern"));

pub type Outcome<T> = Result<T, String>;

/// Whether this machine is OpenWrt.
pub fn openwrt() -> bool {
    fs::exists("/etc/openwrt_release")
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
        let openwrt = openwrt();
        Installer {
            dry_run,
            warnings: Cell::new(0),
            openwrt,
            binary: if openwrt { "/usr/bin/limen" } else { "/usr/local/bin/limen" },
        }
    }

    /// Who the hub logs in as: `limen`, or root on OpenWrt.
    pub fn node_user(&self) -> String {
        if self.openwrt { "root".into() } else { NODE_USER.into() }
    }

    /// Sets this machine up with the hub's [hub_key], which [from] limits. Without a key there is no hub yet: the
    /// binary, the user and the configuration are there, and a join adds the key later.
    pub fn install(&self, hub_key: Option<&str>, from: Option<&str>, announce: bool) -> Outcome<i32> {
        self.check_arguments(hub_key, from)?;
        self.require_root()?;
        if let Some(key) = hub_key {
            self.refuse_key_opened_elsewhere(key)?;
        }
        self.install_binary()?;
        if self.openwrt {
            if let Some(key) = hub_key {
                self.wire_dropbear(key)?;
            }
        } else {
            self.wire_openssh(hub_key, from)?;
        }
        self.ensure_directories()?;
        if announce {
            self.announce_next_steps(hub_key.is_some());
        }
        Ok(0)
    }

    pub fn uninstall(&self, purge: bool) -> Outcome<i32> {
        self.require_root()?;
        if self.openwrt {
            self.unwire_dropbear()?;
        } else {
            self.unwire_openssh()?;
        }
        if purge {
            self.purge()?;
        } else {
            self.note("kept /etc/limen and the logs (--purge removes them)");
        }
        self.remove_file(self.binary)?;
        self.conclude("limen is uninstalled.");
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
    fn check_arguments(&self, hub_key: Option<&str>, from: Option<&str>) -> Outcome<()> {
        if let Some(hub_key) = hub_key {
            validate_key("--hub-key", hub_key)?;
        }
        let Some(from) = from else { return Ok(()) };
        if hub_key.is_none() {
            return Err("--from limits where the hub's key connects from; give --hub-key too".into());
        }
        if !FROM_ADDRESSES.is_match(from) {
            return Err(format!("--from '{from}' is not a list of addresses or CIDRs"));
        }
        if self.openwrt {
            return Err(
                "dropbear has no from= option; limit port 22 with the firewall and run it without --from".into()
            );
        }
        Ok(())
    }

    /// A key that also opens root would take whoever holds it past the gate.
    fn refuse_key_opened_elsewhere(&self, key: &str) -> Outcome<()> {
        let blob = key_blob(key);
        // Each file, and whether limen's own line in it is to be left out.
        let mut key_files: Vec<(&str, bool)> = ROOT_KEY_FILES.map(|path| (path, false)).to_vec();
        if self.openwrt {
            key_files.push((DROPBEAR_KEYS, true));
        }
        match key_files.into_iter().find(|(path, without_limen_line)| lists_key(path, blob, *without_limen_line)) {
            Some((place, _)) => Err(format!(
                "the hub's key already opens {place} without limen's limits; remove it there, or give limen a key of \
                 its own"
            )),
            None => Ok(()),
        }
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

    fn wire_dropbear(&self, key: &str) -> Outcome<()> {
        self.write_dropbear_key(key)?;
        self.write_sysupgrade_keep()?;
        self.check_dropbear();
        Ok(())
    }

    /// The user and its sudo rule always; its key when there is one, which a join adds later otherwise.
    fn wire_openssh(&self, hub_key: Option<&str>, from: Option<&str>) -> Outcome<()> {
        self.ensure_user(NODE_USER)?;
        if let Some(key) = hub_key {
            self.write_authorized_keys(NODE_USER, key, from)?;
        }
        self.write_sudoers()?;
        self.mask_user_manager(NODE_USER)?;
        self.check_sshd(NODE_USER);
        Ok(())
    }

    fn unwire_dropbear(&self) -> Outcome<()> {
        self.remove_dropbear_keys()?;
        self.remove_file(SYSUPGRADE_KEEP)
    }

    fn unwire_openssh(&self) -> Outcome<()> {
        self.unmask_user_manager(NODE_USER)?;
        self.remove_user(NODE_USER)?;
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

    /// The hub's logins need no user manager, and pam_systemd would start one for each, with whatever the machine
    /// starts for every user: sound servers on a desktop, which fail and fill the journal. logind takes a masked
    /// `user@<uid>.service` as none.
    fn mask_user_manager(&self, user: &str) -> Outcome<()> {
        let Some(unit) = user_manager(user) else { return Ok(()) };
        if masked(&unit) {
            return Ok(());
        }
        self.act(&format!("mask {unit}: {user} needs no user manager, and each login would start one"), || {
            exec(&["systemctl", "mask", &unit])
        })
    }

    /// Before the user goes: a uid given to someone else later must not come without a user manager.
    fn unmask_user_manager(&self, user: &str) -> Outcome<()> {
        let Some(unit) = user_manager(user) else { return Ok(()) };
        if !masked(&unit) {
            return Ok(());
        }
        self.act(&format!("unmask {unit}"), || exec(&["systemctl", "unmask", &unit]))
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

    fn write_authorized_keys(&self, user: &str, key: &str, from: Option<&str>) -> Outcome<()> {
        let home = home_of(user);
        let from = from.map(|addresses| format!(",from=\"{addresses}\"")).unwrap_or_default();
        let line = format!("restrict{from},command=\"sudo -n {}\" {}\n", self.gate_command(), key.trim());
        let path = format!("{home}/.ssh/authorized_keys");
        if fs::read_following(&path).as_deref() == Some(line.as_str()) {
            return Ok(());
        }
        // Owned by root: the account itself can't change which key opens it or what that key runs.
        self.act(&format!("write {path} (the hub's key, forced command)"), || {
            let ssh_dir = format!("{home}/.ssh");
            fs::mkdirs(&ssh_dir, 0o755)?;
            fs::chown(&ssh_dir, 0, 0)?;
            fs::chmod(&ssh_dir, 0o755)?;
            fs::write_following(&path, line.as_bytes(), 0o644)?;
            fs::chown(&path, 0, 0)
        })
    }

    /// Root's `authorized_keys` of dropbear, shared with whoever administers the router: only limen's own line (the one
    /// with its forced command) is replaced; every other key stays as it was.
    fn write_dropbear_key(&self, key: &str) -> Outcome<()> {
        let current = fs::read_following(DROPBEAR_KEYS);
        let mut lines: Vec<String> = other_keys(current.as_deref().unwrap_or("")).map(String::from).collect();
        lines.push(format!(
            "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty,command=\"{}\" {}",
            self.gate_command(),
            key.trim()
        ));
        let text = format!("{}\n", lines.join("\n"));
        if current.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.act(&format!("write the hub's key in {DROPBEAR_KEYS} (forced command; other keys kept)"), || {
            fs::mkdirs(fs::parent(DROPBEAR_KEYS), 0o700)?;
            fs::write_following(DROPBEAR_KEYS, text.as_bytes(), 0o600)
        })
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

    fn write_sudoers(&self) -> Outcome<()> {
        let text = format!(
            "# Written by `limen install`. {NODE_USER} may run exactly its gate as root, nothing else.\n\
             # SSH_CONNECTION only, for the audit log; never SSH_ORIGINAL_COMMAND, which limen ignores.\n\
             Defaults:{NODE_USER} env_keep += \"SSH_CONNECTION\"\n\
             {NODE_USER} ALL=(root) NOPASSWD: {}\n",
            self.gate_command()
        );
        if fs::read_following(SUDOERS).as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.act(&format!("write {SUDOERS} (checked with visudo first)"), || {
            check_sudoers(&text)?;
            fs::write_following(SUDOERS, text.as_bytes(), 0o440)?;
            fs::chown(SUDOERS, 0, 0)
        })
    }

    fn ensure_directories(&self) -> Outcome<()> {
        self.ensure_directory("/etc/limen", 0o755, "")?;
        self.write_config()?;
        self.ensure_directory("/var/log/limen", 0o700, " (root only)")
    }

    fn ensure_directory(&self, dir: &str, mode: u32, detail: &str) -> Outcome<()> {
        if fs::is_directory(dir) {
            return Ok(());
        }
        self.act(&format!("create {dir}{detail}"), || fs::mkdirs(dir, mode))
    }

    /// limen.toml, the template, when there is none: the operator's otherwise, which may come from their repository.
    fn write_config(&self) -> Outcome<()> {
        // Anything there, a link to a checkout not made yet included, is the operator's.
        if fs::lstat(node_config::PATH).is_some() {
            return Ok(());
        }
        self.act(&format!("write {} (nothing readable, no packs, until you list them)", node_config::PATH), || {
            fs::write_following(node_config::PATH, CONFIG_TEMPLATE.as_bytes(), 0o644)
        })
    }

    fn purge(&self) -> Outcome<()> {
        for path in ["/etc/limen", "/var/log/limen"].into_iter().filter(|path| fs::exists(path)) {
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

    /// `AllowUsers`/`AllowGroups` that would keep the user out are the classic silent failure: say it.
    fn check_sshd(&self, user: &str) {
        let Some(sshd) = proc::which("sshd") else {
            self.note("sshd not found; limen needs it to be reachable");
            return;
        };
        let Some(effective) = effective_sshd_config(sshd) else { return };
        let settings: Vec<(&str, &str)> = effective.lines().filter_map(|line| line.split_once(' ')).collect();
        if shut_out(user, &settings) {
            self.warn(&format!("sshd has AllowUsers/AllowGroups without {user}: it can't log in"));
        }
        if settings.iter().any(|(key, value)| *key == "pubkeyauthentication" && *value == "no") {
            self.warn("sshd has PubkeyAuthentication no");
        }
    }

    /// What the hub's key is held to: the gate, and nothing else.
    fn gate_command(&self) -> String {
        format!("{} gate", self.binary)
    }

    fn announce_next_steps(&self, with_key: bool) {
        sys::say("");
        if !with_key {
            self.conclude("limen is installed, with no hub yet.");
            sys::say("A hub joins it later: on the hub, `limen invite <name>`; here, as root, the line that prints.");
        } else {
            self.conclude("limen is installed.");
            sys::say(&format!(
                "Next: list what may be read and the packs in {}, then add this node to the hub:",
                node_config::PATH
            ));
            sys::say(&format!("  [nodes.{}]", node_name()));
            sys::say("  host = \"<address>\"");
            if self.openwrt {
                sys::say("  user = \"root\"");
            }
            let host_key = self.host_key().unwrap_or("<ssh-keyscan -t ed25519 this-host, checked out of band>".into());
            sys::say(&format!("  host_key = \"{host_key}\""));
        }
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

/// Whether sshd's `AllowUsers`/`AllowGroups` leave [user] out, if it has either. The user has a group of its own name
/// (`useradd --user-group`), so either list may let it in.
fn shut_out(user: &str, settings: &[(&str, &str)]) -> bool {
    let values = |key: &str| -> Vec<&str> {
        settings.iter().filter(|(name, _)| *name == key).flat_map(|(_, value)| value.split(' ')).collect()
    };
    let (allow_users, allow_groups) = (values("allowusers"), values("allowgroups"));
    let restricted = !allow_users.is_empty() || !allow_groups.is_empty();
    restricted && !allow_users.contains(&user) && !allow_groups.contains(&user)
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

/// [user]'s `user@<uid>.service`, where systemd runs and the user exists.
fn user_manager(user: &str) -> Option<String> {
    if !fs::is_directory("/run/systemd/system") {
        return None;
    }
    fs::account(user).map(|account| user_manager_unit(account.uid))
}

fn user_manager_unit(uid: u32) -> String {
    format!("user@{uid}.service")
}

/// Whether `systemctl mask` linked [unit] to /dev/null.
fn masked(unit: &str) -> bool {
    fs::read_link(&format!("/etc/systemd/system/{unit}")).as_deref() == Some("/dev/null")
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

const CONFIG_TEMPLATE: &str = r#"# limen on this node (docs/spec.md §7.1, docs/scripts.md). Read by every request; no restart needed.

[files]
# Nothing is readable until it is listed here. Whatever is readable ends up in the context of the model
# the hub talks to, so list what helps diagnose and nothing that holds a secret.
allow = [
  # "/etc/nginx/**",
  # "/var/log/nginx/*.log",
]
deny = [
  "**/*.env",
]
# max_bytes = 262144

[scripts]
# The packs this node offers: directories of scripts, each a tool the agent can run. Offer only what you
# would let whoever writes to your logs trigger.
packs = [
  # "/opt/state/packs/systemd",
  # "/opt/state/packs/docker",
]

[redact]
# Names whose values are masked wherever they are assigned, and patterns added to the built-in ones.
names = []
patterns = []

[limits]
# max_lines = 2000
# scan_lines = 100000
# max_response = 1048576
# concurrency = 8
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_configuration_that_allows_nothing() {
        let config = node_config::NodeConfig::parse(CONFIG_TEMPLATE).unwrap();
        assert!(config.allow.is_empty() && config.packs.is_empty());
        assert_eq!(config.deny, ["**/*.env"]);
    }

    #[test]
    fn the_user_manager_is_the_unit_of_the_uid() {
        assert_eq!(user_manager_unit(996), "user@996.service");
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
