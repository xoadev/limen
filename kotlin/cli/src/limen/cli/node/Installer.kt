package limen.cli.node

import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.LimenException
import limen.core.Role
import limen.core.config.GitHub
import limen.core.config.NodeConfig
import limen.core.config.RepoConfig
import limen.core.toml.TomlException
import kotlin.time.Duration.Companion.seconds

/** `--repo`, `--branch` and `--path` of `install`. */
class RepoOptions(
    val url: String,
    val branch: String,
    val path: String,
)

/**
 * `limen install`, `uninstall` and `token` (spec §3, §10): the wiring that makes the gate the only way in, and the
 * repository the node takes its scripts from. Every step says what it does; `--dry-run` says it without doing it,
 * and running it twice changes nothing the second time.
 *
 * Two wirings. Debian and the like: OpenSSH, one system user per role and a sudo rule for exactly its gate.
 * OpenWrt: dropbear, which has neither sudo nor extra users, so both keys belong to root, each held to its gate by
 * its forced command.
 */
class Installer(
    private val dryRun: Boolean,
) {
    private var warnings = 0
    private val openwrt = Platform.openwrt
    private val binary = if (openwrt) "/usr/bin/limen" else "/usr/local/bin/limen"

    fun install(
        readKey: String,
        deployKey: String?,
        from: String?,
        repo: RepoOptions?,
    ): Int {
        validateKey("--read-key", readKey)
        deployKey?.let { validateKey("--deploy-key", it) }
        if (from != null && !CIDRS.matches(from)) fail("--from '$from' is not a list of addresses or CIDRs")
        if (from != null && openwrt) fail("dropbear has no from= option; limit port 22 with the firewall and run it without --from")
        val repoConfig = repo?.let(::repoConfig)
        requireRoot()
        if (repoConfig != null && Proc.which("git") == null) {
            fail("--repo needs git on this node (${if (openwrt) "the git-http package" else "apt install git"})")
        }

        installBinary()
        val keys = listOfNotNull(Role.READ to readKey, deployKey?.let { Role.DEPLOY to it })
        if (openwrt) {
            writeDropbearKeys(keys)
            writeSysupgradeKeep()
            checkDropbear()
        } else {
            for ((role, key) in keys) {
                ensureUser(userOf(role))
                writeAuthorizedKeys(userOf(role), role, key, from)
            }
            if (deployKey == null && Fs.account(userOf(Role.DEPLOY)) != null) {
                note("${userOf(Role.DEPLOY)} exists from an earlier install; left as it is (uninstall removes it)")
            }
            writeSudoers(keys.map { it.first })
            checkSshd(keys.map { userOf(it.first) })
        }
        ensureDirectories(repo)
        if (repoConfig != null) connectRepo(repoConfig)

        say("")
        say(if (dryRun) "Dry run: nothing was changed." else "limen is installed.")
        say("Next: list what may be read in ${NodeConfig.PATH} ([files].allow is empty), then add this node to the hub:")
        say("  [nodes.${Sys.hostname().lowercase().filter { it.isLetterOrDigit() || it == '-' }}]")
        say("  host = \"<address>\"")
        if (openwrt) say("  user = \"root\"")
        say("  host_key = \"${hostKey() ?: "<ssh-keyscan -t ed25519 this-host, checked out of band>"}\"")
        if (warnings > 0) say("$warnings warning(s) above.")
        return 0
    }

    fun uninstall(purge: Boolean): Int {
        requireRoot()
        val config = runCatching { Node.load(NodeConfig.PATH).config }.getOrNull()
        if (openwrt) {
            removeDropbearKeys()
            if (Fs.exists(KEEP)) act("remove $KEEP") { Fs.remove(KEEP) }
        } else {
            for (role in Role.entries) {
                val user = userOf(role)
                if (Fs.account(user) != null) act("remove user $user and its home") { exec("userdel", "--remove", user) }
            }
            if (Fs.exists(SUDOERS)) act("remove $SUDOERS") { Fs.remove(SUDOERS) }
        }
        if (purge) {
            val repo = config?.repo
            for (path in listOfNotNull("/etc/limen", "/var/log/limen", repo?.dir, repo?.syncRecord)) {
                if (Fs.exists(path)) act("remove $path") { exec("rm", "-rf", "--", path) }
            }
        } else {
            note("kept /etc/limen, the logs and the repository checkout (--purge removes them)")
        }
        if (Fs.exists(binary)) act("remove $binary") { Fs.remove(binary) }
        say(if (dryRun) "Dry run: nothing was changed." else "limen is uninstalled.")
        return 0
    }

    /** `limen token`: asks for the repository token again, checks it and saves it. For when it expires. */
    fun token(): Int {
        requireRoot()
        val repo = Node.load(NodeConfig.PATH).config.repo ?: fail("no [repo] in ${NodeConfig.PATH}")
        askToken(repo, force = true)
        return 0
    }

    private fun repoConfig(options: RepoOptions): RepoConfig =
        try {
            NodeConfig.parse(repoSection(options)).repo!!
        } catch (e: TomlException) {
            fail("--repo: ${e.message}")
        }

    private fun repoSection(options: RepoOptions) =
        "[repo]\nurl = \"${options.url}\"\nbranch = \"${options.branch}\"\npath = \"${options.path.trim('/')}\"\n"

    /** The repository: a token when it needs one, and the first checkout. */
    private fun connectRepo(repo: RepoConfig) {
        if (dryRun) {
            say("would check access to ${repo.displayUrl}, ask for a token if it needs one, and sync it to ${repo.dir}")
            return
        }
        askToken(repo, force = false)
        say("sync ${repo.displayUrl} ${repo.branch} into ${repo.dir}")
        val (_, to) =
            try {
                Repo.sync(Node.load(NodeConfig.PATH), repo)
            } catch (e: LimenException) {
                fail("sync: ${e.message}")
            }
        note("at ${to.take(12)}; `limen apply` runs its setup scripts and stacks")
    }

    private fun askToken(
        repo: RepoConfig,
        force: Boolean,
    ) {
        if (!force) {
            if (Repo.canRead(repo, null) == null) return note("${repo.displayUrl} is readable without a token")
            Repo.token(repo)?.let { if (Repo.canRead(repo, it) == null) return note("the saved token reads ${repo.displayUrl}") }
        }
        if (!repo.url.startsWith("https://")) fail("${repo.displayUrl} is not readable, and a token only works over https://")
        val github = repo.github
        say("")
        say("${repo.displayUrl} needs a token that can read it.")
        if (github != null) {
            say("Create one here (fine-grained, read-only contents, no expiry):")
            say("  ${GitHub.tokenUrl(github.first, github.second, Sys.hostname())}")
            say("In the form, check that the resource owner is ${github.first} (select it again if in doubt) and, under")
            say("Repository access, choose \"Only select repositories\" and ${github.second}.")
        }
        repeat(3) {
            Sys.err("Token: ")
            val token = (if (Sys.isTerminal(0)) Sys.readSecret() else readlnOrNull())?.trim()
            if (token.isNullOrEmpty()) fail("no token given")
            val problem = Repo.canRead(repo, token)
            if (problem == null) {
                Fs.mkdirs(repo.tokenFile.substringBeforeLast('/'), 0b111_101_101)
                Fs.writeAtomic(repo.tokenFile, (token + "\n").encodeToByteArray(), 0b110_000_000)
                Fs.chown(repo.tokenFile, 0, 0)
                say("token saved in ${repo.tokenFile} (root only)")
                return
            }
            say("that token can't read ${repo.displayUrl}: $problem")
        }
        fail("no token that reads ${repo.displayUrl}")
    }

    private fun installBinary() {
        val self = Fs.realPath("/proc/self/exe") ?: fail("cannot find this binary")
        if (self == binary) return
        val bytes = Fs.read(self) ?: fail("cannot read $self")
        val current = Fs.realPath(binary)?.let { Fs.read(it) }
        if (current != null && current.contentEquals(bytes)) return
        act("install this binary as $binary") {
            Fs.mkdirs(binary.substringBeforeLast('/'), 0b111_101_101)
            Fs.writeAtomic(binary, bytes, 0b111_101_101)
            Fs.chown(binary, 0, 0)
        }
    }

    private fun ensureUser(user: String) {
        val home = "/var/lib/$user"
        val existing = Fs.account(user)
        if (existing == null) {
            act("create system user $user (home $home, shell /bin/sh)") {
                exec("useradd", "--system", "--shell", "/bin/sh", "--home-dir", home, "--create-home", "--user-group", user)
            }
        }
        // `/bin/sh` and not `nologin`: sshd runs the forced command through the login shell, and with `nologin`
        // nothing runs. `*` and not `!`: a locked password makes sshd refuse even key logins when PAM is off.
        if (existing == null || existing.shell != "/bin/sh" || shadowPassword(user) != "*") {
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
        val home = Fs.account(user)?.home ?: "/var/lib/$user"
        val options =
            buildString {
                append("restrict")
                if (from != null) append(",from=\"$from\"")
                append(",command=\"sudo -n $binary gate --role ${role.wire}\"")
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

    /**
     * Root's `authorized_keys` of dropbear, shared with whoever administers the router: only limen's own lines (those
     * with its forced command) are replaced; every other key stays as it was.
     */
    private fun writeDropbearKeys(keys: List<Pair<Role, String>>) {
        val current = Fs.readText(DROPBEAR_KEYS)
        val others = current?.lines()?.filter { it.isNotBlank() && LIMEN_LINE !in it }.orEmpty()
        val ours =
            keys.map { (role, key) ->
                "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty,command=\"$binary gate --role ${role.wire}\" ${key.trim()}"
            }
        val text = (others + ours).joinToString("\n", postfix = "\n")
        if (current == text) return
        act("write limen's keys in $DROPBEAR_KEYS (${keys.joinToString { it.first.wire }}, forced commands; other keys kept)") {
            Fs.mkdirs("/etc/dropbear", 0b111_000_000)
            Fs.writeAtomic(DROPBEAR_KEYS, text.encodeToByteArray(), 0b110_000_000)
        }
    }

    private fun removeDropbearKeys() {
        val lines = Fs.readText(DROPBEAR_KEYS)?.lines()?.filter { it.isNotBlank() } ?: return
        val kept = lines.filter { LIMEN_LINE !in it }
        if (kept.size == lines.size) return
        act("remove limen's keys from $DROPBEAR_KEYS (other keys kept)") {
            val text = if (kept.isEmpty()) "" else kept.joinToString("\n", postfix = "\n")
            Fs.writeAtomic(DROPBEAR_KEYS, text.encodeToByteArray(), 0b110_000_000)
        }
    }

    /** A system upgrade of OpenWrt keeps only the files listed in keep.d: limen's are its binary and /etc/limen. */
    private fun writeSysupgradeKeep() {
        val text = "$binary\n/etc/limen/\n"
        if (Fs.stat(KEEP.substringBeforeLast('/'))?.type != FileType.DIRECTORY || Fs.readText(KEEP) == text) return
        act("write $KEEP (sysupgrade keeps limen)") { Fs.writeAtomic(KEEP, text.encodeToByteArray(), 0b110_100_100) }
    }

    private fun writeSudoers(roles: List<Role>) {
        val text =
            buildString {
                append("# Written by `limen install`. Each limen user may run exactly its gate as root, nothing else.\n")
                for (role in roles) {
                    val user = userOf(role)
                    // SSH_CONNECTION only, for the audit log; never SSH_ORIGINAL_COMMAND, which limen ignores.
                    append("Defaults:$user env_keep += \"SSH_CONNECTION\"\n")
                    append("$user ALL=(root) NOPASSWD: $binary gate --role ${role.wire}\n")
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

    private fun ensureDirectories(repo: RepoOptions?) {
        for (dir in listOf("/etc/limen", "/etc/limen/checks.d", "/etc/limen/actions.d", "/etc/limen/setup.d")) {
            if (Fs.stat(dir)?.type != FileType.DIRECTORY) act("create $dir") { Fs.mkdirs(dir, 0b111_101_101) }
        }
        val existing = Fs.readText(NodeConfig.PATH)
        when {
            existing == null -> {
                act("write ${NodeConfig.PATH} (nothing readable until you list it)") {
                    val text = CONFIG_TEMPLATE + "\n" + (repo?.let(::repoSection) ?: REPO_TEMPLATE)
                    Fs.writeAtomic(NodeConfig.PATH, text.encodeToByteArray(), 0b110_100_100)
                }
            }

            repo != null -> {
                val current = runCatching { NodeConfig.parse(existing).repo }.getOrNull()
                when {
                    current == null && Regex("(?m)^\\s*\\[repo]").containsMatchIn(existing) -> {
                        warn("${NodeConfig.PATH} has a [repo] section limen can't read; left as it is")
                    }

                    current == null -> {
                        act("add [repo] to ${NodeConfig.PATH}") {
                            val text = existing.trimEnd() + "\n\n" + repoSection(repo)
                            Fs.writeAtomic(NodeConfig.PATH, text.encodeToByteArray(), 0b110_100_100)
                        }
                    }

                    current.url != repo.url || current.branch != repo.branch || current.path != repo.path.trim('/') -> {
                        warn("${NodeConfig.PATH} already has another [repo] (${current.displayUrl}); left as it is")
                    }
                }
            }
        }
        for (dir in listOf("/var/log/limen", "/var/log/limen/runs")) {
            if (Fs.stat(dir)?.type != FileType.DIRECTORY) act("create $dir (root only)") { Fs.mkdirs(dir, 0b111_000_000) }
        }
    }

    /**
     * A forced command only holds a key-based login. With root's password empty —as OpenWrt ships— dropbear lets
     * anyone who reaches it in without a key, and limen's limits mean nothing: say it.
     */
    private fun checkDropbear() {
        if (shadowPassword("root")?.isEmpty() == true) {
            warn("root has no password: dropbear lets anyone in without a key. Set one (passwd), or turn password logins off")
        }
        val uci = Proc.which("uci") ?: return
        val passwordAuth =
            runCatching { Proc.run(listOf(uci, "-q", "get", "dropbear.@dropbear[0].PasswordAuth")) }.getOrNull()?.out?.trim()
        if (passwordAuth != "off" && passwordAuth != "0") {
            note(
                "dropbear accepts passwords; with keys only, nothing but limen's keys and yours gets in: " +
                    "uci set dropbear.@dropbear[0].PasswordAuth=off; uci set dropbear.@dropbear[0].RootPasswordAuth=off; " +
                    "uci commit dropbear; service dropbear restart",
            )
        }
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

    private fun hostKey(): String? {
        if (openwrt) {
            val dropbearkey = Proc.which("dropbearkey") ?: return null
            val r = runCatching { Proc.run(listOf(dropbearkey, "-y", "-f", "/etc/dropbear/dropbear_ed25519_host_key")) }.getOrNull()
            return r
                ?.out
                ?.lineSequence()
                ?.firstOrNull { it.startsWith("ssh-ed25519 ") }
                ?.split(' ')
                ?.take(2)
                ?.joinToString(" ")
        }
        return Fs
            .readText("/etc/ssh/ssh_host_ed25519_key.pub")
            ?.trim()
            ?.split(' ')
            ?.take(2)
            ?.joinToString(" ")
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
            say(what)
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
        const val SUDOERS = "/etc/sudoers.d/limen"
        const val DROPBEAR_KEYS = "/etc/dropbear/authorized_keys"
        const val KEEP = "/lib/upgrade/keep.d/limen"

        /** What marks limen's lines in a shared authorized_keys: its forced command. */
        private const val LIMEN_LINE = "limen gate --role"

        fun userOf(role: Role) = "limen-${role.wire}"

        private val PUBLIC_KEY =
            Regex(
                "^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\\.com|sk-ecdsa-sha2-nistp256@openssh\\.com) " +
                    "[A-Za-z0-9+/]+=*( [^\\r\\n\"]*)?$",
            )
        private val CIDRS = Regex("^[0-9A-Fa-f.:/*?,!]+$")

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

            """.trimIndent()

        private val REPO_TEMPLATE =
            """
            # The repository this node takes its scripts, stacks and node.toml from (`limen install --repo` fills it).
            # [repo]
            # url = "https://github.com/<owner>/<repo>.git"
            # branch = "main"
            # path = "nodes/<this node>"

            """.trimIndent()
    }
}
