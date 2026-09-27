package limen.cli.node

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.files.Glob
import limen.core.system.Parsers
import limen.core.system.ProcFs
import limen.core.system.ProcSample
import kotlin.time.Duration.Companion.seconds
import kotlin.time.Instant

enum class Init(
    val wire: String,
) {
    SYSTEMD("systemd"),
    PROCD("procd"),
    NONE("none"),
}

/**
 * What differs between the machines limen runs on (spec §11): the init system and its log, and OpenWrt. What
 * the kernel says the same way everywhere —processes, sockets, mounts— is read from `/proc` directly, never through
 * `ps`, `ss` or `df`, whose busybox versions lack the options.
 */
object Platform {
    val openwrt: Boolean get() = Fs.exists("/etc/openwrt_release")

    val init: Init
        get() =
            when {
                Fs.exists("/run/systemd/system") -> Init.SYSTEMD
                openwrt -> Init.PROCD
                else -> Init.NONE
            }

    /** The board, where the system says it (OpenWrt). */
    fun board(): String? = Fs.readText("/tmp/sysinfo/model")?.trim()?.ifEmpty { null }

    fun disks(): JsonArray {
        val mounts = ProcFs.mounts(Fs.readText("/proc/self/mounts") ?: throw LimenException(ErrorCode.UNAVAILABLE, "no /proc/self/mounts"))
        return buildJsonArray {
            for (m in mounts) {
                // A file bind-mounted over another (a container's /etc/hostname) is not a filesystem to report.
                if (Fs.stat(m.point)?.type != FileType.DIRECTORY) continue
                val (free, total) = Fs.space(m.point) ?: continue
                if (total == 0L) continue
                val used = total - free
                add(
                    buildJsonObject {
                        put("mount", m.point)
                        put("device", m.device)
                        put("fstype", m.type)
                        put("size_bytes", total)
                        put("used_bytes", used)
                        put("available_bytes", free)
                        put("used_percent", (used * 1000 / total) / 10.0)
                    },
                )
            }
        }
    }

    fun processes(
        node: Node,
        byMemory: Boolean,
        limit: Int,
    ): JsonArray {
        val uptime = Fs.readText("/proc/uptime")?.substringBefore(' ')?.toDoubleOrNull() ?: 0.0
        val memTotalKb =
            Fs
                .readText("/proc/meminfo")
                ?.lineSequence()
                ?.firstOrNull { it.startsWith("MemTotal:") }
                ?.split(Regex("\\s+"))
                ?.getOrNull(1)
                ?.toLongOrNull() ?: 0
        val ticks = Sys.ticksPerSecond()
        val users = Fs.accounts().associate { it.uid to it.name }
        val samples = pids().mapNotNull(::sample)
        val sorted =
            if (byMemory) {
                samples.sortedByDescending { it.rssKb }
            } else {
                samples.sortedByDescending { ProcFs.cpuPercent(it, uptime, ticks) }
            }
        return buildJsonArray {
            for (p in sorted.take(limit)) {
                add(
                    buildJsonObject {
                        put("pid", p.pid)
                        put("user", p.uid?.let { users[it] ?: it.toString() })
                        put("cpu_percent", ProcFs.cpuPercent(p, uptime, ticks))
                        put("memory_percent", if (memTotalKb > 0) (p.rssKb * 1000 / memTotalKb) / 10.0 else null)
                        put("rss_bytes", p.rssKb * 1024)
                        put("elapsed_seconds", (uptime - p.startTicks.toDouble() / ticks).toLong().coerceAtLeast(0))
                        put("command", node.redactor.redact(p.cmdline).take(500))
                    },
                )
            }
        }
    }

    fun ports(): JsonArray {
        val sockets =
            listOf("tcp", "tcp6", "udp", "udp6")
                .flatMap { proto ->
                    Fs.readText("/proc/net/$proto")?.let { ProcFs.sockets(it, proto) }.orEmpty()
                }.filter { it.listening }
        // Which process holds each socket: /proc/<pid>/fd/* point to `socket:[<inode>]`. Only root sees them all.
        val owners = mutableMapOf<Long, MutableList<Pair<Int, String>>>()
        for (pid in pids()) {
            val fds = runCatching { Fs.list("/proc/$pid/fd") }.getOrNull() ?: continue
            val comm = Fs.readText("/proc/$pid/comm")?.trim() ?: continue
            for (fd in fds) {
                val target = Fs.readLink("/proc/$pid/fd/$fd") ?: continue
                if (!target.startsWith("socket:[")) continue
                val inode = target.removePrefix("socket:[").removeSuffix("]").toLongOrNull() ?: continue
                owners.getOrPut(inode) { mutableListOf() } += pid to comm
            }
        }
        return buildJsonArray {
            for (s in sockets.sortedWith(compareBy({ it.protocol }, { it.port }))) {
                add(
                    buildJsonObject {
                        put("protocol", s.protocol)
                        put("address", s.address)
                        put("port", s.port)
                        putJsonArray("processes") {
                            owners[s.inode].orEmpty().distinct().forEach { (pid, comm) ->
                                add(
                                    buildJsonObject {
                                        put("name", comm)
                                        put("pid", pid)
                                    },
                                )
                            }
                        }
                    },
                )
            }
        }
    }

    private fun pids(): List<Int> = Fs.list("/proc").mapNotNull { it.toIntOrNull() }

    private fun sample(pid: Int): ProcSample? {
        val stat = Fs.readText("/proc/$pid/stat") ?: return null
        val status = Fs.readText("/proc/$pid/status") ?: return null
        val cmdline = Fs.read("/proc/$pid/cmdline", 8192)?.decodeToString().orEmpty()
        return ProcFs.sample(pid, stat, status, cmdline)
    }

    /** Services that should run and don't: failed units (systemd), or procd services with no running instance. */
    fun failedServices(node: Node): List<String> =
        when (init) {
            Init.SYSTEMD -> {
                Parsers
                    .units(node.execOk("systemctl", "list-units", "--failed", "--no-legend", "--plain", "--no-pager"))
                    .map { it.jsonObject["unit"]!!.jsonPrimitive.content }
            }

            Init.PROCD -> {
                procdServices(node).filter { it.value.state == ProcdState.FAILED }.keys.toList()
            }

            Init.NONE -> {
                throw LimenException(ErrorCode.UNAVAILABLE, "no systemd or procd on this node")
            }
        }

    enum class ProcdState(
        val active: String,
        val sub: String,
    ) {
        RUNNING("active", "running"),
        CONFIGURED("active", "exited"),
        FAILED("failed", "dead"),
    }

    class ProcdService(
        val state: ProcdState,
        val instances: JsonObject,
    )

    /** `ubus call service list`: every procd service and its instances. */
    fun procdServices(
        node: Node,
        name: String? = null,
    ): Map<String, ProcdService> {
        val args =
            if (name == null) {
                "{}"
            } else {
                buildJsonObject {
                    put("name", name)
                    put("verbose", true)
                }.toString()
            }
        val out = node.execOk("ubus", "call", "service", "list", args)
        val all = runCatching { LenientJson.parseToJsonElement(out.ifBlank { "{}" }).jsonObject }.getOrElse { JsonObject(emptyMap()) }
        return all.mapValues { (_, v) ->
            val instances = (v as? JsonObject)?.get("instances") as? JsonObject ?: JsonObject(emptyMap())
            val running = instances.values.map { (it as? JsonObject)?.get("running")?.jsonPrimitive?.booleanOrNull == true }
            val state =
                when {
                    running.isEmpty() -> ProcdState.CONFIGURED
                    running.any { it } -> ProcdState.RUNNING
                    else -> ProcdState.FAILED
                }
            ProcdService(state, instances)
        }
    }

    /** Whether `/etc/rc.d` starts [name] at boot, as `service <name> enabled` says on OpenWrt. */
    fun procdEnabled(name: String): Boolean =
        runCatching { Fs.list("/etc/rc.d") }.getOrDefault(emptyList()).any { it.matches(Regex("^S\\d+${Regex.escape(name)}$")) }

    fun procdUnits(
        node: Node,
        state: String?,
        pattern: String?,
    ): JsonArray =
        buildJsonArray {
            for ((name, s) in procdServices(node).entries.sortedBy { it.key }) {
                if (pattern != null && !Glob.segment(pattern, name)) continue
                val keep =
                    when (state) {
                        null, "all" -> true
                        "running" -> s.state == ProcdState.RUNNING
                        "failed" -> s.state == ProcdState.FAILED
                        "active" -> s.state != ProcdState.FAILED
                        "exited" -> s.state == ProcdState.CONFIGURED
                        else -> false
                    }
                if (!keep) continue
                add(
                    buildJsonObject {
                        put("unit", name)
                        put("load", "loaded")
                        put("active", s.state.active)
                        put("sub", s.state.sub)
                        put("enabled", procdEnabled(name))
                        put("instances", s.instances.size)
                    },
                )
            }
        }

    fun procdService(
        node: Node,
        name: String,
        lines: Int,
    ): JsonObject {
        val s =
            procdServices(node, name)[name]
                ?: if (Fs.exists("/etc/init.d/$name")) {
                    ProcdService(ProcdState.CONFIGURED, JsonObject(emptyMap()))
                } else {
                    throw LimenException(ErrorCode.NOT_FOUND, "no service named $name")
                }
        return buildJsonObject {
            put("name", name)
            put("active", s.state.active)
            put("sub", s.state.sub)
            put("enabled", procdEnabled(name))
            putJsonArray("instances") {
                for ((instance, v) in s.instances) {
                    val o = v as? JsonObject ?: continue
                    add(
                        buildJsonObject {
                            put("name", instance)
                            put("running", o["running"] ?: JsonPrimitive(false))
                            put("pid", o["pid"]?.jsonPrimitive?.longOrNull)
                            put("exit_code", o["exit_code"]?.jsonPrimitive?.longOrNull)
                            val command = (o["command"] as? JsonArray)?.mapNotNull { it.jsonPrimitive.contentOrNull }?.joinToString(" ")
                            put("command", command?.let(node.redactor::redact))
                        },
                    )
                }
            }
            put("journal", JsonArray(if (lines > 0) logread(node, lines, source = name) else emptyList()))
        }
    }

    /**
     * The OpenWrt log (`logread`), filtered here: by [source] (the program, as `unit` names it), by priority, by
     * time and by [grep]. The last [lines] that match, oldest first.
     */
    fun logread(
        node: Node,
        lines: Int,
        source: String? = null,
        priority: String? = null,
        since: Instant? = null,
        until: Instant? = null,
        grep: String? = null,
    ): List<JsonObject> {
        val filtered = source != null || priority != null || since != null || until != null || grep != null
        val r = node.exec("logread", "-l", (if (filtered) node.config.scanLines else lines).toString(), timeout = 60.seconds)
        if (r.exitCode !=
            0
        ) {
            throw LimenException(
                ErrorCode.UNAVAILABLE,
                "logread: ${r.err
                    .trim()
                    .lines()
                    .lastOrNull() ?: "exit ${r.exitCode}"}",
            )
        }
        val maxLevel = priority?.let { Parsers.priorityNumber(it) }
        return r.out
            .lineSequence()
            .filter { it.isNotEmpty() }
            .mapNotNull { line ->
                val e = ProcFs.logreadLine(line)
                if (e == null) {
                    return@mapNotNull if (source == null && maxLevel == null && since == null && until == null &&
                        (grep == null || line.contains(grep, ignoreCase = true))
                    ) {
                        buildJsonObject { put("message", node.redactor.redact(line)) }
                    } else {
                        null
                    }
                }
                if (source != null && e.source != source && !e.source.startsWith("$source-")) return@mapNotNull null
                if (maxLevel != null && Parsers.priorityNumber(e.priority).let { it < 0 || it > maxLevel }) return@mapNotNull null
                val time = runCatching { Instant.parse(e.time) }.getOrNull()
                if (since != null && (time == null || time < since)) return@mapNotNull null
                if (until != null && (time == null || time > until)) return@mapNotNull null
                if (grep != null && !e.message.contains(grep, ignoreCase = true)) return@mapNotNull null
                buildJsonObject {
                    put("time", e.time)
                    put("priority", e.priority)
                    put("source", e.source)
                    put("pid", e.pid)
                    put("message", node.redactor.redact(e.message))
                }
            }.toList()
            .takeLast(lines)
    }
}
