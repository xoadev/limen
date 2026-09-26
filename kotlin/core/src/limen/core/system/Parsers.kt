package limen.core.system

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import limen.core.LenientJson
import limen.core.Redactor
import kotlin.time.Instant

/**
 * What the gate reads from the programs it runs, turned into the JSON it answers. Pure functions of their text, so
 * each format is tested with a captured sample and no machine.
 */
object Parsers {
    private val whitespace = Regex("\\s+")

    /** `systemctl show` output: one `Key=Value` per line. */
    fun keyValues(text: String): Map<String, String> =
        text.lineSequence().filter { '=' in it }.associate { it.substringBefore('=') to it.substringAfter('=') }

    /** `systemctl list-units --no-legend --plain`: UNIT LOAD ACTIVE SUB DESCRIPTION. */
    fun units(text: String): JsonArray =
        buildJsonArray {
            for (line in text.lineSequence().map { it.trim() }.filter { it.isNotEmpty() }) {
                val parts = line.split(whitespace, limit = 5)
                if (parts.size < 4) continue
                add(
                    buildJsonObject {
                        put("unit", parts[0])
                        put("load", parts[1])
                        put("active", parts[2])
                        put("sub", parts[3])
                        put("description", parts.getOrElse(4) { "" })
                    },
                )
            }
        }

    /** The service view of `systemctl show`: the properties an agent needs, with systemd's "unset" as null. */
    fun unit(props: Map<String, String>): JsonObject =
        buildJsonObject {
            put("name", props["Id"])
            put("description", props["Description"])
            put("load", props["LoadState"])
            put("active", props["ActiveState"])
            put("sub", props["SubState"])
            put("result", props["Result"])
            put("enabled", props["UnitFileState"]?.ifEmpty { null })
            put("unit_file", props["FragmentPath"]?.ifEmpty { null })
            put("type", props["Type"]?.ifEmpty { null })
            put("restart", props["Restart"]?.ifEmpty { null })
            put("main_pid", props["MainPID"]?.toLongOrNull()?.takeIf { it > 0 })
            put("exit_status", props["ExecMainStatus"]?.toLongOrNull())
            put("restarts", props["NRestarts"]?.toLongOrNull())
            put("memory_bytes", props["MemoryCurrent"]?.toULongOrNull()?.takeIf { it != ULong.MAX_VALUE }?.toLong())
            put("active_since", props["ActiveEnterTimestamp"]?.ifEmpty { null })
            put("state_changed", props["StateChangeTimestamp"]?.ifEmpty { null })
        }

    private val priorities = listOf("emerg", "alert", "crit", "err", "warning", "notice", "info", "debug")

    fun priorityNumber(name: String): Int = priorities.indexOf(name)

    /** `journalctl -o json`: one object per line. `MESSAGE` may be an array of bytes when it is not valid UTF-8. */
    fun journal(
        text: String,
        redactor: Redactor,
    ): List<JsonObject> =
        text
            .lineSequence()
            .filter { it.isNotBlank() }
            .mapNotNull { line ->
                val o = runCatching { LenientJson.parseToJsonElement(line).jsonObject }.getOrNull() ?: return@mapNotNull null
                val micros = o.str("__REALTIME_TIMESTAMP")?.toLongOrNull()
                buildJsonObject {
                    put("time", micros?.let { iso(it / 1_000_000) })
                    put("priority", o.str("PRIORITY")?.toIntOrNull()?.let { priorities.getOrNull(it) })
                    put("source", o.str("SYSLOG_IDENTIFIER") ?: o.str("_COMM"))
                    put("pid", o.str("_PID")?.toLongOrNull())
                    o.str("_SYSTEMD_UNIT")?.let { put("unit", it) }
                    put("message", redactor.redact(message(o["MESSAGE"])))
                }
            }.toList()

    private fun message(value: JsonElement?): String =
        when (value) {
            null, JsonNull -> ""
            is JsonPrimitive -> value.content
            is JsonArray -> value.mapNotNull { it.jsonPrimitive.intOrNull?.toByte() }.toByteArray().decodeToString()
            else -> value.toString()
        }

    /** `df -B1 --output=source,fstype,size,used,avail,target`, header included. */
    fun disks(text: String): JsonArray =
        buildJsonArray {
            for (line in text
                .lineSequence()
                .drop(1)
                .map { it.trim() }
                .filter { it.isNotEmpty() }) {
                val p = line.split(whitespace, limit = 6)
                if (p.size < 6) continue
                val size = p[2].toLongOrNull() ?: continue
                val used = p[3].toLongOrNull() ?: continue
                add(
                    buildJsonObject {
                        put("mount", p[5])
                        put("device", p[0])
                        put("fstype", p[1])
                        put("size_bytes", size)
                        put("used_bytes", used)
                        put("available_bytes", p[4].toLongOrNull())
                        put("used_percent", if (size > 0) (used * 1000 / size) / 10.0 else 0.0)
                    },
                )
            }
        }

    /** `/proc/meminfo`, in bytes. */
    fun memory(text: String): JsonObject {
        val kb =
            text.lineSequence().associate { line ->
                line.substringBefore(':') to
                    line
                        .substringAfter(':')
                        .trim()
                        .split(' ')
                        .first()
                        .toLongOrNull()
            }

        fun bytes(key: String) = kb[key]?.times(1024)
        return buildJsonObject {
            put("total_bytes", bytes("MemTotal"))
            put("available_bytes", bytes("MemAvailable"))
            put("swap_total_bytes", bytes("SwapTotal"))
            put("swap_free_bytes", bytes("SwapFree"))
        }
    }

    /** `/etc/os-release`'s `PRETTY_NAME`. */
    fun osName(text: String): String? =
        text
            .lineSequence()
            .firstOrNull { it.startsWith("PRETTY_NAME=") }
            ?.substringAfter('=')
            ?.trim()
            ?.removeSurrounding("\"")

    private val ssProcess = Regex("\\(\"([^\"]*)\",pid=(\\d+)")

    /** `ss -H -tulnp`: Netid State Recv-Q Send-Q Local Peer [Process]. */
    fun ports(text: String): JsonArray =
        buildJsonArray {
            for (line in text.lineSequence().map { it.trim() }.filter { it.isNotEmpty() }) {
                val p = line.split(whitespace, limit = 7)
                if (p.size < 5) continue
                val local = p[4]
                val address = local.substringBeforeLast(':')
                val port = local.substringAfterLast(':').toIntOrNull()
                add(
                    buildJsonObject {
                        put("protocol", p[0])
                        put("address", address)
                        put("port", port)
                        put(
                            "processes",
                            buildJsonArray {
                                ssProcess.findAll(p.getOrElse(6) { "" }).distinctBy { it.groupValues[2] }.forEach { m ->
                                    add(
                                        buildJsonObject {
                                            put("name", m.groupValues[1])
                                            put("pid", m.groupValues[2].toLong())
                                        },
                                    )
                                }
                            },
                        )
                    },
                )
            }
        }

    /** `ps -eo pid=,uid=,pcpu=,pmem=,rss=,etimes=,args=`, already sorted and cut. */
    fun processes(
        text: String,
        userName: (Int) -> String?,
        redactor: Redactor,
    ): JsonArray =
        buildJsonArray {
            for (line in text.lineSequence().map { it.trim() }.filter { it.isNotEmpty() }) {
                val p = line.split(whitespace, limit = 7)
                if (p.size < 7) continue
                val uid = p[1].toIntOrNull()
                add(
                    buildJsonObject {
                        put("pid", p[0].toLongOrNull())
                        put("user", uid?.let(userName) ?: p[1])
                        put("cpu_percent", p[2].toDoubleOrNull())
                        put("memory_percent", p[3].toDoubleOrNull())
                        put("rss_bytes", p[4].toLongOrNull()?.times(1024))
                        put("elapsed_seconds", p[5].toLongOrNull())
                        put("command", redactor.redact(p[6]).take(500))
                    },
                )
            }
        }

    /** The list view of one `docker inspect` object. */
    fun containerSummary(o: JsonObject): JsonObject {
        val state = o.obj("State")
        val labels = o.obj("Config")?.obj("Labels")
        return buildJsonObject {
            put("name", o.str("Name")?.removePrefix("/"))
            put("image", o.obj("Config")?.str("Image"))
            put("state", state?.str("Status"))
            put("health", state?.obj("Health")?.str("Status"))
            put("restarts", o["RestartCount"]?.jsonPrimitive?.longOrNull)
            put("started", state?.str("StartedAt"))
            put("compose_project", labels?.str("com.docker.compose.project"))
            put("compose_service", labels?.str("com.docker.compose.service"))
        }
    }

    /** The detail view of one `docker inspect` object: never an environment value, only names (spec §5). */
    fun containerDetail(
        o: JsonObject,
        digests: List<String>,
        redactor: Redactor,
    ): JsonObject {
        val state = o.obj("State")
        val config = o.obj("Config")
        val health = state?.obj("Health")
        return buildJsonObject {
            put("id", o.str("Id")?.take(12))
            put("name", o.str("Name")?.removePrefix("/"))
            put("image", config?.str("Image"))
            put("image_id", o.str("Image"))
            put("image_digests", JsonArray(digests.map(::JsonPrimitive)))
            put("created", o.str("Created"))
            put(
                "state",
                buildJsonObject {
                    put("status", state?.str("Status"))
                    put("running", state?.get("Running") ?: JsonNull)
                    put("started_at", state?.str("StartedAt"))
                    put("finished_at", state?.str("FinishedAt"))
                    put("exit_code", state?.get("ExitCode") ?: JsonNull)
                    put("oom_killed", state?.get("OOMKilled") ?: JsonNull)
                    put("error", state?.str("Error")?.ifEmpty { null })
                    if (health != null) {
                        put(
                            "health",
                            buildJsonObject {
                                put("status", health.str("Status"))
                                put("failing_streak", health["FailingStreak"] ?: JsonNull)
                                put(
                                    "log",
                                    buildJsonArray {
                                        health.arr("Log").orEmpty().takeLast(5).forEach { entry ->
                                            val e = entry.jsonObject
                                            add(
                                                buildJsonObject {
                                                    put("start", e.str("Start"))
                                                    put("exit_code", e["ExitCode"] ?: JsonNull)
                                                    put("output", redactor.redact(e.str("Output").orEmpty().trim()).take(500))
                                                },
                                            )
                                        }
                                    },
                                )
                            },
                        )
                    }
                },
            )
            put("restart_count", o["RestartCount"] ?: JsonNull)
            put("restart_policy", o.obj("HostConfig")?.obj("RestartPolicy")?.str("Name"))
            val command = listOfNotNull(o.str("Path")) + o.arr("Args").orEmpty().mapNotNull { it.jsonPrimitive.contentOrNull }
            put("command", redactor.redact(command.joinToString(" ")))
            put(
                "mounts",
                buildJsonArray {
                    o.arr("Mounts").orEmpty().forEach { m ->
                        val mo = m.jsonObject
                        add(
                            buildJsonObject {
                                put("type", mo.str("Type"))
                                put("source", mo.str("Source"))
                                put("destination", mo.str("Destination"))
                                put("rw", mo["RW"] ?: JsonNull)
                            },
                        )
                    }
                },
            )
            put("ports", o.obj("NetworkSettings")?.get("Ports") ?: JsonNull)
            put(
                "networks",
                buildJsonObject {
                    o.obj("NetworkSettings")?.obj("Networks")?.forEach { (name, net) ->
                        put(name, buildJsonObject { put("ip", net.jsonObject.str("IPAddress")?.ifEmpty { null }) })
                    }
                },
            )
            put("labels", config?.obj("Labels") ?: JsonObject(emptyMap()))
            put(
                "env",
                JsonArray(
                    config
                        ?.arr("Env")
                        .orEmpty()
                        .mapNotNull { it.jsonPrimitive.contentOrNull?.substringBefore('=') }
                        .map(::JsonPrimitive),
                ),
            )
        }
    }

    /**
     * `docker logs --timestamps` lines: `2026-09-26T08:00:00.123456789Z message`. [stream] is `stdout` or `stderr`,
     * which Docker gives on separate pipes; the timestamp is what merges them back in order.
     */
    fun dockerLogLines(
        text: String,
        stream: String,
    ): List<Triple<String, String, String>> =
        text
            .lineSequence()
            .filter { it.isNotEmpty() }
            .map { line ->
                val time = line.substringBefore(' ')
                Triple(time, stream, if (' ' in line) line.substringAfter(' ') else "")
            }.toList()

    /** Seconds since the epoch as `2026-09-26T08:00:00Z`. */
    fun iso(epochSeconds: Long): String = Instant.fromEpochSeconds(epochSeconds).toString()

    private fun JsonObject.str(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull

    private fun JsonObject.obj(key: String): JsonObject? = this[key] as? JsonObject

    private fun JsonObject.arr(key: String): JsonArray? = this[key] as? JsonArray

    fun parseArray(text: String): JsonArray = LenientJson.parseToJsonElement(text).jsonArray
}
