package limen.core.system

/**
 * What the kernel says in `/proc` and `/etc`, parsed without the programs that usually read it (`ps`, `ss`, `df`,
 * `getpwuid`): they differ between distributions and busybox, and a static binary can't use glibc's NSS.
 */
data class Account(
    val name: String,
    val uid: Int,
    val gid: Int,
    val home: String,
    val shell: String,
)

data class Mount(
    val device: String,
    val point: String,
    val type: String,
)

/** One socket of `/proc/net/{tcp,udp}{,6}`. */
data class NetSocket(
    val protocol: String,
    val address: String,
    val port: Int,
    val listening: Boolean,
    val inode: Long,
)

/** The fields of `/proc/<pid>/stat` and `status` that `processes` shows. */
data class ProcSample(
    val pid: Int,
    val comm: String,
    val uid: Int?,
    val utimeTicks: Long,
    val stimeTicks: Long,
    val startTicks: Long,
    val rssKb: Long,
    val cmdline: String,
)

object ProcFs {
    /** `/etc/passwd`. */
    fun accounts(text: String): List<Account> =
        text
            .lineSequence()
            .mapNotNull { line ->
                val f = line.split(':')
                if (f.size < 7 || line.startsWith("#")) return@mapNotNull null
                Account(f[0], f[2].toIntOrNull() ?: return@mapNotNull null, f[3].toIntOrNull() ?: 0, f[5], f[6])
            }.toList()

    /** `/etc/group`: gid → name. */
    fun groups(text: String): Map<Int, String> =
        text
            .lineSequence()
            .mapNotNull { line ->
                val f = line.split(':')
                if (f.size < 3) null else f[2].toIntOrNull()?.let { it to f[0] }
            }.toMap()

    private val pseudo =
        setOf(
            "proc",
            "sysfs",
            "devtmpfs",
            "devpts",
            "tmpfs",
            "cgroup",
            "cgroup2",
            "securityfs",
            "pstore",
            "bpf",
            "debugfs",
            "tracefs",
            "mqueue",
            "hugetlbfs",
            "configfs",
            "fusectl",
            "autofs",
            "efivarfs",
            "binfmt_misc",
            "nsfs",
            "rpc_pipefs",
            "ramfs",
            "squashfs",
            "fuse.portal",
            "fuse.gvfsd-fuse",
            "nfsd",
        )

    /**
     * The mounts of `/proc/mounts` that hold data: no pseudo-filesystems, no container layers (an `overlay` only at
     * `/` or `/overlay`, which is where OpenWrt keeps its writable root), each device once.
     */
    fun mounts(text: String): List<Mount> {
        val points = mutableSetOf<String>()
        val devices = mutableSetOf<String>()
        return text
            .lineSequence()
            .mapNotNull { line ->
                val f = line.split(' ')
                if (f.size < 3) return@mapNotNull null
                val m = Mount(unescape(f[0]), unescape(f[1]), f[2])
                if (m.type in pseudo || (m.type == "overlay" && m.point != "/" && m.point != "/overlay")) return@mapNotNull null
                // Both sets learn from every real mount, so a device seen at a hidden mount point still hides its binds.
                val newPoint = points.add(m.point)
                val newDevice = m.type == "overlay" || devices.add(m.device)
                if (newPoint && newDevice) m else null
            }.toList()
    }

    /** `\040` and the other octal escapes of `/proc/mounts`. */
    private fun unescape(s: String) =
        Regex("\\\\([0-7]{3})").replace(s) {
            it.groupValues[1]
                .toInt(8)
                .toChar()
                .toString()
        }

    /** `/proc/net/tcp`, `tcp6`, `udp` or `udp6` ([protocol] is the file name). */
    fun sockets(
        text: String,
        protocol: String,
    ): List<NetSocket> =
        text
            .lineSequence()
            .drop(1)
            .mapNotNull { line ->
                val f = line.trim().split(Regex("\\s+"))
                if (f.size < 10) return@mapNotNull null
                val (hexAddress, hexPort) = f[1].split(':').takeIf { it.size == 2 } ?: return@mapNotNull null
                val state = f[3]
                // TCP listens in state 0A; a bound UDP socket shows 07 (closed: no peer).
                val listening = if (protocol.startsWith("tcp")) state == "0A" else state == "07"
                NetSocket(protocol, address(hexAddress), hexPort.toInt(16), listening, f[9].toLongOrNull() ?: 0)
            }.toList()

    /** The kernel prints addresses as 32-bit words in host (little-endian) order. */
    fun address(hex: String): String {
        val words = hex.chunked(8).map { word -> word.chunked(2).reversed().map { it.toInt(16) } }
        return if (words.size == 1) {
            words[0].joinToString(".")
        } else {
            val bytes = words.flatten()
            ipv6(bytes.chunked(2).map { (it[0] shl 8) or it[1] })
        }
    }

    private fun ipv6(groups: List<Int>): String {
        // The longest run of zero groups becomes `::`.
        var bestStart = -1
        var bestLength = 0
        var i = 0
        while (i < groups.size) {
            if (groups[i] == 0) {
                val start = i
                while (i < groups.size && groups[i] == 0) i++
                if (i - start > bestLength && i - start > 1) {
                    bestStart = start
                    bestLength = i - start
                }
            } else {
                i++
            }
        }
        val hex = groups.map { it.toString(16) }
        if (bestStart < 0) return hex.joinToString(":")
        val head = hex.subList(0, bestStart).joinToString(":")
        val tail = hex.subList(bestStart + bestLength, hex.size).joinToString(":")
        return "$head::$tail"
    }

    /** `/proc/<pid>/stat`, `status` and `cmdline` of one process; null when it vanished or is unreadable. */
    fun sample(
        pid: Int,
        stat: String,
        status: String,
        cmdline: String,
    ): ProcSample? {
        val open = stat.indexOf('(')
        val close = stat.lastIndexOf(')')
        if (open < 0 || close < open) return null
        val comm = stat.substring(open + 1, close)
        // After the command: state is field 3 of the man page, index 0 here.
        val f = stat.substring(close + 2).split(' ')
        if (f.size < 20) return null
        val uid =
            status
                .lineSequence()
                .firstOrNull { it.startsWith("Uid:") }
                ?.split(Regex("\\s+"))
                ?.getOrNull(1)
                ?.toIntOrNull()
        val rss =
            status
                .lineSequence()
                .firstOrNull { it.startsWith("VmRSS:") }
                ?.split(Regex("\\s+"))
                ?.getOrNull(1)
                ?.toLongOrNull() ?: 0
        val command = cmdline.trimEnd('\u0000').replace('\u0000', ' ').ifEmpty { "[$comm]" }
        return ProcSample(pid, comm, uid, f[11].toLongOrNull() ?: 0, f[12].toLongOrNull() ?: 0, f[19].toLongOrNull() ?: 0, rss, command)
    }

    /** Average CPU use over the life of the process, as `ps` computes `%CPU`. */
    fun cpuPercent(
        p: ProcSample,
        uptimeSeconds: Double,
        ticksPerSecond: Long,
    ): Double {
        val alive = uptimeSeconds - p.startTicks.toDouble() / ticksPerSecond
        if (alive <= 0) return 0.0
        val used = (p.utimeTicks + p.stimeTicks).toDouble() / ticksPerSecond
        return (used / alive * 1000).toLong() / 10.0
    }

    /**
     * One `logread` line (OpenWrt): `Sat Sep 26 17:00:00 2026 daemon.info dnsmasq[1234]: message`. The time is the
     * one logread prints, UTC when it runs with `TZ=UTC`.
     */
    fun logreadLine(line: String): LogreadEntry? {
        val g = LOGREAD.matchEntire(line)?.groupValues ?: return null
        val month = MONTHS.indexOf(g[1]) + 1
        if (month == 0) return null
        return LogreadEntry(
            time = "${g[4]}-${month.toString().padStart(2, '0')}-${g[2].padStart(2, '0')}T${g[3]}Z",
            priority = if (g[6] == "warn") "warning" else g[6],
            source = g[7].trim(),
            pid = g[8].toLongOrNull(),
            message = g[9],
        )
    }

    private val MONTHS = listOf("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec")

    // weekday month day time year facility.level source[pid]: message
    private val LOGREAD =
        Regex("^\\w{3} (\\w{3}) +(\\d{1,2}) (\\d\\d:\\d\\d:\\d\\d) (\\d{4}) (\\w+)\\.(\\w+) ([^\\[:]+?)(?:\\[(\\d+)\\])?: ?(.*)$")
}

data class LogreadEntry(
    val time: String,
    val priority: String,
    val source: String,
    val pid: Long?,
    val message: String,
)
