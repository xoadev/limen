package limen.core.scripts

/** What limen needs to know of a file or directory to decide whether to trust it. */
data class FileStat(
    val path: String,
    val uid: Int,
    val mode: Int,
    val isDirectory: Boolean,
    val isRegular: Boolean,
)

/**
 * Whether a script may run (spec §6): the file and every directory above it owned by root or by [owner] —the user
 * limen runs as, root in production— and not writable by group or others: `sshd`'s `StrictModes` rule. Otherwise
 * whoever can write there can make the gate run anything as root.
 */
object Trust {
    private const val GROUP_OR_OTHER_WRITE = 0b000_010_010
    private const val OWNER_EXECUTE = 0b001_000_000

    /** [chain] is the script first, then each parent up to `/`. Null when trusted, otherwise the reason. */
    fun problem(
        chain: List<FileStat>,
        owner: Int,
    ): String? {
        val file = chain.firstOrNull() ?: return "not found"
        if (!file.isRegular) return "${file.path} is not a regular file"
        if (file.mode and OWNER_EXECUTE == 0) return "${file.path} is not executable"
        for (entry in chain) {
            if (entry.uid != 0 &&
                entry.uid != owner
            ) {
                return "${entry.path} is not owned by ${if (owner == 0) "root" else "root or uid $owner"}"
            }
            if (entry.mode and GROUP_OR_OTHER_WRITE != 0) return "${entry.path} is writable by group or others"
        }
        return null
    }
}
