package limen.cli.node

import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.Role
import limen.core.config.NodeConfig
import kotlin.time.Duration.Companion.seconds

/**
 * `limen install` and `limen uninstall` (spec §3, §10): the wiring that makes the gate the only way in. Every step
 * says what it does; `--dry-run` says it without doing it, and running it twice changes nothing the second time.
 */
class Installer(
    private val dryRun: Boolean,
) {
    private var warnings = 0

    fun install(
        readKey: String,
        deployKey: String?,
        from: String?,
    ): Int {
        validateKey("--read-key", readKey)
        deployKey?.let { validateKey("--deploy-key", it) }
        if (from != null && !CIDRS.matches(from)) fail("--from '$from' is not a list of addresses or CIDRs")
        requireRoot()

        installBinary()
        val roles = listOfNotNull(Role.READ to readKey, deployKey?.let { Role.DEPLOY to it })
        for ((role, key) in roles) {
            val user = userOf(role)
            ensureUser(user)
            writeAuthorizedKeys(user, role, key, from)
        }
        if (deployKey == null && Fs.user(userOf(Role.DEPLOY)) != null) {
            note("${userOf(Role.DEPLOY)} exists from an earlier install; left as it is (uninstall removes it)")
        }
        writeSudoers(roles.map { it.first })
        ensureDirectories()
        writeLogrotate()
        checkSshd(roles.map { userOf(it.first) })

        val hostKey =
            Fs
                .readText("/etc/ssh/ssh_host_ed25519_key.pub")
                ?.trim()
                ?.split(' ')
                ?.take(2)
                ?.joinToString(" ")
        say("")
        say(if (dryRun) "Dry run: nothing was changed." else "limen is installed.")
        say("Next: list what may be read in ${NodeConfig.PATH} ([files].allow is empty), then add this node to the hub:")
        say("  [nodes.${Sys.hostname().lowercase().filter { it.isLetterOrDigit() || it == '-' }}]")
        say("  host = \"<address>\"")
        say("  host_key = \"${hostKey ?: "<ssh-keyscan -t ed25519 this-host, checked out of band>"}\"")
        if (warnings > 0) say("$warnings warning(s) above.")
        return 0
    }

    fun uninstall(purge: Boolean): Int {
        requireRoot()
        for (role in Role.entries) {
            val user = userOf(role)
            if (Fs.user(user) != null) act("remove user $user and its home") { exec("userdel", "--remove", user) }
        }
        for (file in listOf(SUDOERS, LOGROTATE)) {
            if (Fs.exists(file)) act("remove $file") { Fs.remove(file) }
        }
        if (purge) {
            for (dir in listOf("/etc/limen", "/var/log/limen")) {
                if (Fs.exists(dir)) act("remove $dir") { exec("rm", "-rf", "--", dir) }
            }
        } else {
            note("kept /etc/limen and /var/log/limen (--purge removes them)")
        }
        if (Fs.exists(BINARY)) act("remove $BINARY") { Fs.remove(BINARY) }
        say(if (dryRun) "Dry run: nothing was changed." else "limen is uninstalled.")
        return 0
    }

    private fun installBinary() {
        val self = Fs.realPath("/proc/self/exe") ?: fail("cannot find this binary")
        if (self == BINARY) return
        val bytes = Fs.read(self) ?: fail("cannot read $self")
        val current = Fs.realPath(BINARY)?.let { Fs.read(it) }
        if (current != null && current.contentEquals(bytes)) return
        act("install this binary as $BINARY") {
            Fs.mkdirs(BINARY.substringBeforeLast('/'), 0b111_101_101)
            Fs.writeAtomic(BINARY, bytes, 0b111_101_101)
            Fs.chown(BINARY, 0, 0)
        }
    }

    private fun ensureUser(user: String) {
        val home = "/var/lib/$user"
        val existing = Fs.user(user)
        if (existing == null) {
            act("create system user $user (home $home, shell /bin/sh)") {
                exec("useradd", "--system", "--shell", "/bin/sh", "--home-dir", home, "--create-home", "--user-group", user)
            }
        }
        // `/bin/sh` and not `nologin`: sshd runs the forced command through the login shell, and with `nologin`
        // nothing runs. `*` and not `!`: a locked password makes sshd refuse even key logins when PAM is off.
        if (existing == null || Fs.shell(user) != "/bin/sh" || shadowPassword(user) != "*") {
            act("set $user's shell to /bin/sh and its password to none (key login only)") {
                exec("usermod", "--shell", "/bin/sh", "--password", "*", user)
            }
        }
    }

    private fun shadowPassword(user: String): String? =
        Fs
            .readText("/etc/shadow")
            ?.lineSequence()
            ?.firstOrNull { it.startsWith("$user:") }
            ?.split(':')
            ?.getOrNull(1)

    private fun writeAuthorizedKeys(
        user: String,
        role: Role,
        key: String,
        from: String?,
    ) {
        val home = Fs.user(user)?.third ?: "/var/lib/$user"
        val options =
            buildString {
                append("restrict")
                if (from != null) append(",from=\"$from\"")
                append(",command=\"sudo -n $BINARY gate --role ${role.wire}\"")
            }
        val line = "$options ${key.trim()}\n"
        val path = "$home/.ssh/authorized_keys"
        if (Fs.realPath(path)?.let { Fs.readText(it) } == line) return
        // Owned by root: the account itself can't change which key opens it or what that key runs.
        act("write $path (${role.wire} role, forced command)") {
            Fs.mkdirs("$home/.ssh", 0b111_101_101)
            Fs.chown("$home/.ssh", 0, 0)
            Fs.chmod("$home/.ssh", 0b111_101_101)
            Fs.writeAtomic(path, line.encodeToByteArray(), 0b110_100_100)
            Fs.chown(path, 0, 0)
        }
    }

    private fun writeSudoers(roles: List<Role>) {
        val text =
            buildString {
                append("# Written by `limen install`. Each limen user may run exactly its gate as root, nothing else.\n")
                for (role in roles) {
                    val user = userOf(role)
                    // SSH_CONNECTION only, for the audit log; never SSH_ORIGINAL_COMMAND, which limen ignores.
                    append("Defaults:$user env_keep += \"SSH_CONNECTION\"\n")
                    append("$user ALL=(root) NOPASSWD: $BINARY gate --role ${role.wire}\n")
                }
            }
        if (Fs.realPath(SUDOERS)?.let { Fs.readText(it) } == text) return
        act("write $SUDOERS (checked with visudo first)") {
            // A name with a dot: sudo skips it, so a half-written check file is never in force.
            val check = "/etc/sudoers.d/limen.check"
            Fs.writeAtomic(check, text.encodeToByteArray(), 0b100_100_000)
            val r = Proc.run(listOf(Proc.which("visudo") ?: fail("visudo is not installed"), "-cf", check))
            Fs.remove(check)
            if (r.exitCode != 0) fail("visudo rejected the sudoers file: ${r.err.trim()}${r.out.trim()}")
            Fs.writeAtomic(SUDOERS, text.encodeToByteArray(), 0b100_100_000)
            Fs.chown(SUDOERS, 0, 0)
        }
    }

    private fun ensureDirectories() {
        for (dir in listOf("/etc/limen", "/etc/limen/checks.d", "/etc/limen/actions.d", "/etc/limen/setup.d")) {
            if (Fs.stat(dir)?.type != FileType.DIRECTORY) act("create $dir") { Fs.mkdirs(dir, 0b111_101_101) }
        }
        if (!Fs.exists(NodeConfig.PATH)) {
            act("write ${NodeConfig.PATH} (nothing readable until you list it)") {
                Fs.writeAtomic(NodeConfig.PATH, CONFIG_TEMPLATE.encodeToByteArray(), 0b110_100_100)
            }
        }
        for (dir in listOf("/var/log/limen", "/var/log/limen/runs")) {
            if (Fs.stat(dir)?.type != FileType.DIRECTORY) act("create $dir (root only)") { Fs.mkdirs(dir, 0b111_000_000) }
        }
    }

    private fun writeLogrotate() {
        if (Fs.stat("/etc/logrotate.d")?.type != FileType.DIRECTORY) return
        if (Fs.realPath(LOGROTATE)?.let { Fs.readText(it) } == LOGROTATE_TEXT) return
        act("write $LOGROTATE") { Fs.writeAtomic(LOGROTATE, LOGROTATE_TEXT.encodeToByteArray(), 0b110_100_100) }
    }

    /** `AllowUsers`/`AllowGroups` that would keep the new users out are the classic silent failure: say it. */
    private fun checkSshd(users: List<String>) {
        val sshd = Proc.which("sshd") ?: return note("sshd not found; limen needs it to be reachable")
        val r = runCatching { Proc.run(listOf(sshd, "-T"), timeout = 10.seconds) }.getOrNull() ?: return
        if (r.exitCode != 0) return
        val settings =
            r.out
                .lines()
                .map { it.split(' ', limit = 2) }
                .filter { it.size == 2 }
        val allowUsers = settings.filter { it[0] == "allowusers" }.flatMap { it[1].split(' ') }
        val allowGroups = settings.filter { it[0] == "allowgroups" }.flatMap { it[1].split(' ') }
        if (allowUsers.isNotEmpty() || allowGroups.isNotEmpty()) {
            val missing = users.filter { it !in allowUsers && it !in allowGroups }
            if (missing.isNotEmpty()) warn("sshd has AllowUsers/AllowGroups without ${missing.joinToString()}: they can't log in")
        }
        if (settings.any { it[0] == "pubkeyauthentication" && it[1] == "no" }) warn("sshd has PubkeyAuthentication no")
    }

    private fun requireRoot() {
        if (!dryRun && Sys.euid() != 0) fail("run it as root (or with --dry-run to see what it would do)")
    }

    private fun validateKey(
        option: String,
        key: String,
    ) {
        if (!PUBLIC_KEY.matches(key.trim())) fail("$option is not an SSH public key ('ssh-ed25519 AAAA… comment')")
    }

    private fun exec(vararg argv: String) {
        val path = Proc.which(argv[0]) ?: fail("${argv[0]} is not installed")
        val r = Proc.run(listOf(path) + argv.drop(1), timeout = 60.seconds)
        if (r.exitCode != 0) fail("${argv.joinToString(" ")}: ${r.err.trim()}")
    }

    private fun act(
        what: String,
        block: () -> Unit,
    ) {
        if (dryRun) {
            say("would $what")
        } else {
            say("$what")
            block()
        }
    }

    private fun note(text: String) = say("  note: $text")

    private fun warn(text: String) {
        warnings++
        say("  WARNING: $text")
    }

    private fun say(text: String) = Sys.out("$text\n")

    private fun fail(message: String): Nothing = throw InstallException(message)

    class InstallException(
        message: String,
    ) : Exception(message)

    companion object {
        const val BINARY = "/usr/local/bin/limen"
        const val SUDOERS = "/etc/sudoers.d/limen"
        const val LOGROTATE = "/etc/logrotate.d/limen"

        fun userOf(role: Role) = "limen-${role.wire}"

        private val PUBLIC_KEY =
            Regex(
                "^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\\.com|sk-ecdsa-sha2-nistp256@openssh\\.com) [A-Za-z0-9+/]+=*( [^\\r\\n\"]*)?$",
            )
        private val CIDRS = Regex("^[0-9A-Fa-f.:/*?,!]+$")

        private val LOGROTATE_TEXT =
            """
            /var/log/limen/audit.jsonl {
                weekly
                rotate 12
                compress
                missingok
                notifempty
                create 0600 root root
            }

            """.trimIndent()

        val CONFIG_TEMPLATE =
            """
            # limen on this node (docs/spec.md §7.1). Read by every request; no restart needed.

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

            [scripts]
            # checks = "/etc/limen/checks.d"
            # actions = "/etc/limen/actions.d"
            # setup = "/etc/limen/setup.d"

            """.trimIndent()
    }
}
