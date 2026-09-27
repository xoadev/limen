package limen.core.files

/**
 * What `read_file`, `list_dir` and file logs may open (spec §7.1). Always given the **resolved** path —symlinks and
 * `..` gone— because matching the text the client sent would let a link walk out of the allowlist.
 */
class PathPolicy(
    allow: List<String>,
    deny: List<String>,
) {
    private val allow = allow.map(::Glob)
    private val deny = deny.map(::Glob)

    sealed interface Decision {
        data object Allowed : Decision

        data class Denied(
            val reason: String,
        ) : Decision
    }

    fun check(resolved: String): Decision {
        BUILT_IN_DENY.firstOrNull { it.matches(resolved) }?.let { return Decision.Denied("$resolved is never readable") }
        deny.firstOrNull { it.matches(resolved) }?.let { return Decision.Denied("$resolved is denied by files.deny ($it)") }
        if (allow.none { it.matches(resolved) }) return Decision.Denied("$resolved is not in files.allow")
        return Decision.Allowed
    }

    fun allowed(resolved: String): Boolean = check(resolved) == Decision.Allowed

    /** A directory that is not allowed itself but on the way to something that is: listable, to find it. */
    fun leadsTo(dir: String): Boolean =
        BUILT_IN_DENY.none { it.matches(dir) } &&
            deny.none { it.matches(dir) } &&
            allow.any { it.mayMatchBelow(dir) }

    companion object {
        /** An absolute path with `.` and `..` resolved as text, for a path that doesn't exist and so has no realpath. */
        fun normalize(path: String): String {
            val out = ArrayDeque<String>()
            for (segment in path.split('/')) {
                when (segment) {
                    "", "." -> Unit
                    ".." -> out.removeLastOrNull()
                    else -> out.addLast(segment)
                }
            }
            return "/" + out.joinToString("/")
        }

        /**
         * Can't be overridden by any configuration (spec §7.1): account and sudo secrets, private keys, VPN and
         * Wi-Fi credentials, limen's own directory, root's home, and the pseudo-filesystems, where a "file" can be
         * a process's environment or a whole disk.
         */
        val BUILT_IN_DENY: List<Glob> =
            listOf(
                "/etc/shadow",
                "/etc/shadow-",
                "/etc/gshadow",
                "/etc/gshadow-",
                "/etc/sudoers*",
                "/etc/sudoers.d/**",
                "/etc/ssh/ssh_host_*_key",
                "/etc/dropbear/dropbear_*_host_key",
                "**/.ssh/id_*",
                "/etc/ssl/private/**",
                "/etc/wireguard/**",
                "/etc/NetworkManager/system-connections/**",
                "/etc/limen/**",
                "/root/**",
                "/proc/**",
                "/sys/**",
                "/dev/**",
            ).map(::Glob)
    }
}
