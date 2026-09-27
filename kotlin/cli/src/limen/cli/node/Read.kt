package limen.cli.node

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.Durations
import limen.core.ErrorCode
import limen.core.LIMEN_VERSION
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.PROTOCOL_VERSIONS
import limen.core.Requests
import limen.core.WireJson
import limen.core.badRequest
import limen.core.bool
import limen.core.files.PathPolicy
import limen.core.int
import limen.core.obj
import limen.core.scripts.Catalog
import limen.core.scripts.ScriptKind
import limen.core.string
import limen.core.system.Parsers
import kotlin.time.Instant

/** The read requests (spec §5). Each takes validated arguments and answers JSON. */
object Read {
    fun hello(node: Node): Answer {
        val (kernel, arch) = Sys.uname()
        return Answer(
            buildJsonObject {
                put("version", LIMEN_VERSION)
                put("protocols", JsonArray(PROTOCOL_VERSIONS.map(::JsonPrimitive)))
                put("hostname", Sys.hostname())
                put("os", Fs.realPath("/etc/os-release")?.let { Fs.readText(it) }?.let(Parsers::osName))
                put("kernel", kernel)
                put("arch", arch)
                put("init", Platform.init.wire)
                Platform.board()?.let { put("board", it) }
                put("docker", Proc.which("docker") != null)
                put("repo", node.config.repo != null)
                put("catalog", WireJson.encodeToJsonElement(Catalog.serializer(), Scripts.catalog(node)))
            },
        )
    }

    fun status(node: Node): Answer {
        val errors = linkedMapOf<String, String>()

        fun <T> part(
            name: String,
            block: () -> T,
        ): T? =
            try {
                block()
            } catch (e: Exception) {
                errors[name] = e.message ?: e::class.simpleName ?: "error"
                null
            }
        val uptime =
            part("uptime") {
                Fs
                    .readText("/proc/uptime")!!
                    .substringBefore(' ')
                    .toDouble()
                    .toLong()
            }
        val load =
            part("load") {
                Fs
                    .readText("/proc/loadavg")!!
                    .split(' ')
                    .take(3)
                    .map { it.toDouble() }
            }
        val memory = part("memory") { Parsers.memory(Fs.readText("/proc/meminfo")!!) }
        val disks = part("disks") { Platform.disks() }
        val failed = part("failed_services") { Platform.failedServices(node).map(::JsonPrimitive) }
        val containers =
            if (Proc.which("docker") == null) {
                null
            } else {
                part("containers") {
                    inspectAll(node).map(Parsers::containerSummary).filter {
                        it["state"]?.jsonPrimitive?.contentOrNull != "running" ||
                            it["health"]?.jsonPrimitive?.contentOrNull == "unhealthy"
                    }
                }
            }
        return Answer(
            buildJsonObject {
                put("hostname", Sys.hostname())
                put("uptime_seconds", uptime)
                put("load", load?.let { l -> JsonArray(l.map(::JsonPrimitive)) } ?: JsonNull)
                put("memory", memory ?: JsonNull)
                put("disks", disks ?: JsonNull)
                put("failed_services", failed?.let(::JsonArray) ?: JsonNull)
                put("containers_attention", containers?.let(::JsonArray) ?: JsonNull)
                put("reboot_required", Fs.exists("/run/reboot-required"))
                if (errors.isNotEmpty()) put("errors", JsonObject(errors.mapValues { JsonPrimitive(it.value) }))
            },
        )
    }

    fun services(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        when (Platform.init) {
            Init.PROCD -> return Answer(Platform.procdUnits(node, args.string("state"), args.string("pattern")))
            Init.NONE -> throw LimenException(ErrorCode.UNAVAILABLE, "no systemd or procd on this node")
            Init.SYSTEMD -> Unit
        }
        val argv = mutableListOf("systemctl", "list-units", "--no-legend", "--plain", "--no-pager")
        args.string("type")?.takeIf { it != "all" }?.let { argv += "--type=$it" }
        when (val state = args.string("state")) {
            "all" -> argv += "--all"
            "inactive" -> argv += listOf("--all", "--state=inactive")
            null -> Unit
            else -> argv += "--state=$state"
        }
        args.string("pattern")?.let { argv += listOf("--", it) }
        return Answer(Parsers.units(node.execOk(*argv.toTypedArray())))
    }

    fun service(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val raw = args.string("name")!!
        when (Platform.init) {
            Init.PROCD -> return Answer(Platform.procdService(node, raw.removeSuffix(".service"), args.int("lines") ?: 20))
            Init.NONE -> throw LimenException(ErrorCode.UNAVAILABLE, "no systemd or procd on this node")
            Init.SYSTEMD -> Unit
        }
        val name = if ('.' in raw) raw else "$raw.service"
        val props =
            Parsers.keyValues(
                node.execOk(
                    "systemctl",
                    "show",
                    "--no-pager",
                    "--property=Id,Description,LoadState,ActiveState,SubState,Result,UnitFileState,FragmentPath,MainPID," +
                        "ExecMainStatus,NRestarts,MemoryCurrent,ActiveEnterTimestamp,StateChangeTimestamp,Type,Restart",
                    "--",
                    name,
                ),
            )
        if (props["LoadState"] == "not-found") throw LimenException(ErrorCode.NOT_FOUND, "no unit named $name")
        val lines = args.int("lines") ?: 20
        val journal =
            if (lines > 0) {
                Parsers.journal(node.execOk("journalctl", "-u", name, "-n", "$lines", "-o", "json", "--no-pager", "-q"), node.redactor)
            } else {
                emptyList()
            }
        return Answer(
            buildJsonObject {
                Parsers.unit(props).forEach { (k, v) -> put(k, v) }
                put("journal", JsonArray(journal))
            },
        )
    }

    fun containers(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val all = args.bool("all") ?: true
        val list = inspectAll(node).map(Parsers::containerSummary)
        return Answer(JsonArray(if (all) list else list.filter { it["state"]?.jsonPrimitive?.contentOrNull == "running" }))
    }

    fun container(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val name = args.string("name")!!
        val r = node.exec("docker", "inspect", "--type", "container", "--", name)
        if (r.exitCode != 0) {
            val message = r.err.trim()
            if ("No such" in message) throw LimenException(ErrorCode.NOT_FOUND, "no container named $name")
            throw dockerError(message)
        }
        val o = Parsers.parseArray(r.out).firstOrNull()?.jsonObject ?: throw LimenException(ErrorCode.NOT_FOUND, "no container named $name")
        val imageId = o["Image"]?.jsonPrimitive?.contentOrNull
        val digests =
            imageId
                ?.let { id ->
                    val img = node.exec("docker", "image", "inspect", "--format", "{{json .RepoDigests}}", "--", id)
                    if (img.exitCode == 0) {
                        runCatching { LenientJson.parseToJsonElement(img.out.trim()) as JsonArray }
                            .getOrNull()
                            ?.mapNotNull { it.jsonPrimitive.contentOrNull }
                    } else {
                        null
                    }
                }.orEmpty()
        return Answer(Parsers.containerDetail(o, digests, node.redactor))
    }

    private fun inspectAll(node: Node): List<JsonObject> {
        val ids = node.exec("docker", "ps", "-aq", "--no-trunc")
        if (ids.exitCode != 0) throw dockerError(ids.err.trim())
        val list =
            ids.out
                .lines()
                .map { it.trim() }
                .filter { it.isNotEmpty() }
        if (list.isEmpty()) return emptyList()
        val r = node.exec("docker", "inspect", "--type", "container", *list.toTypedArray())
        if (r.exitCode != 0 && r.out.isBlank()) throw dockerError(r.err.trim())
        return Parsers.parseArray(r.out).map { it.jsonObject }
    }

    private fun dockerError(message: String) =
        if ("Cannot connect" in message || "permission denied" in message) {
            LimenException(ErrorCode.UNAVAILABLE, "docker: ${message.lines().last()}")
        } else {
            LimenException(ErrorCode.INTERNAL, "docker: ${message.lines().lastOrNull() ?: "failed"}")
        }

    fun logs(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val source = args.string("source")!!
        val name = args.string("name")
        val requested = args.int("lines") ?: 200
        val lines = minOf(requested, node.config.maxLines)
        val clamped = lines < requested
        val grep = args.string("grep")
        val since = args.string("since")?.let { instant(node, it) }
        val until = args.string("until")?.let { instant(node, it) }
        val answer =
            when (source) {
                "unit", "journal" -> {
                    if (source == "journal" && name != null) badRequest("source journal takes no name")
                    if (source == "unit" && name == null) badRequest("source unit needs a name")
                    if (Platform.init != Init.SYSTEMD) {
                        if (Platform.init == Init.NONE) throw LimenException(ErrorCode.UNAVAILABLE, "no journal or logread on this node")
                        val entries =
                            Platform.logread(
                                node,
                                lines,
                                name?.removeSuffix(".service"),
                                args.string("priority"),
                                since,
                                until,
                                grep,
                            )
                        return Answer(JsonArray(entries), clamped)
                    }
                    val argv = mutableListOf("journalctl", "-o", "json", "--no-pager", "-q", "-n", "$lines")
                    if (source == "unit") {
                        val unit = name ?: badRequest("source unit needs a name")
                        if (!Regex(Requests.UNIT).matches(unit)) badRequest("'$unit' is not a unit name")
                        argv += listOf("-u", unit)
                    } else if (name != null) {
                        badRequest("source journal takes no name")
                    }
                    since?.let { argv += "--since=${journalTime(it)}" }
                    until?.let { argv += "--until=${journalTime(it)}" }
                    args.string("priority")?.let { argv += "--priority=$it" }
                    grep?.let { argv += listOf("--grep=${pcreLiteral(it)}", "--case-sensitive=false") }
                    val r = node.exec(*argv.toTypedArray(), timeout = kotlin.time.Duration.parse("60s"))
                    // journalctl --grep exits 1 when nothing matches: that is an empty answer, not an error.
                    if (r.exitCode != 0 && r.out.isBlank() && r.err.isNotBlank()) {
                        throw LimenException(ErrorCode.INTERNAL, "journalctl: ${r.err.trim().lines().last()}")
                    }
                    Answer(JsonArray(Parsers.journal(r.out, node.redactor)), r.truncated)
                }

                "container" -> {
                    containerLogs(node, name ?: badRequest("source container needs a name"), lines, grep, since, until)
                }

                "file" -> {
                    if (since != null || until != null) badRequest("since and until do not apply to files")
                    fileLogs(node, name ?: badRequest("source file needs a name"), lines, grep)
                }

                else -> {
                    badRequest("unknown source $source")
                }
            }
        return if (clamped) Answer(answer.data, truncated = true) else answer
    }

    private fun containerLogs(
        node: Node,
        name: String,
        lines: Int,
        grep: String?,
        since: Instant?,
        until: Instant?,
    ): Answer {
        if (!Regex(Requests.CONTAINER).matches(name)) badRequest("'$name' is not a container name")
        val tail = if (grep == null) lines else node.config.scanLines
        val argv = mutableListOf("docker", "logs", "--timestamps", "--tail", "$tail")
        since?.let { argv += "--since=${dockerTime(it)}" }
        until?.let { argv += "--until=${dockerTime(it)}" }
        argv += listOf("--", name)
        val r = node.exec(*argv.toTypedArray(), timeout = kotlin.time.Duration.parse("60s"))
        if (r.exitCode != 0) {
            if ("No such container" in r.err) throw LimenException(ErrorCode.NOT_FOUND, "no container named $name")
            throw dockerError(r.err.trim())
        }
        val merged =
            (Parsers.dockerLogLines(r.out, "stdout") + Parsers.dockerLogLines(r.err, "stderr"))
                .sortedBy { it.first }
                .filter { grep == null || it.third.contains(grep, ignoreCase = true) }
                .takeLast(lines)
        return Answer(
            JsonArray(
                merged.map { (time, stream, message) ->
                    buildJsonObject {
                        put("time", time)
                        put("stream", stream)
                        put("message", node.redactor.redact(message))
                    }
                },
            ),
            r.truncated,
        )
    }

    private fun fileLogs(
        node: Node,
        name: String,
        lines: Int,
        grep: String?,
    ): Answer {
        val path = allowedFile(node, name)
        val scan = if (grep == null) lines else node.config.scanLines
        val slice = Fs.tail(path, scan, maxBytes = minOf(scan.toLong() * 1024, MAX_SCAN_BYTES), exact = true)
        if (slice.binary) badRequest("$path is binary")
        // Redacted before it is cut into lines: a private key the window holds whole spans several of them.
        val redacted = node.redactor.redact(slice.lines.joinToString("\n")).split('\n')
        val matched = redacted.filter { grep == null || it.contains(grep, ignoreCase = true) }.takeLast(lines)
        return Answer(
            buildJsonObject {
                put("path", path)
                put("lines", JsonArray(matched.map(::JsonPrimitive)))
            },
        )
    }

    fun readFile(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val path = allowedFile(node, args.string("path")!!)
        val from = args.int("from") ?: 1
        val count = args.int("lines") ?: 500
        val info = Fs.stat(path)!!
        val slice = Fs.readLines(path, from, count, node.config.maxFileBytes, exact = true)
        if (slice.binary) {
            return Answer(
                buildJsonObject {
                    put("path", path)
                    put("size_bytes", info.size)
                    put("binary", true)
                },
            )
        }
        val cutByBytes = !slice.eof && slice.lines.size < count
        return Answer(
            buildJsonObject {
                put("path", path)
                put("size_bytes", info.size)
                put("from", from)
                put("to", from + slice.lines.size - 1)
                put("eof", slice.eof)
                put("content", node.redactor.redact(slice.lines.joinToString("\n")))
            },
            truncated = cutByBytes,
        )
    }

    fun listDir(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val listable = { path: String -> node.policy.allowed(path) || node.policy.leadsTo(path) }
        val dir = resolve(node, args.string("path")!!, listable)
        // The policy before the type: what a denied path is, like whether it exists, is not the client's to learn.
        if (!listable(dir)) throw LimenException(ErrorCode.DENIED, denial(node, dir))
        if (Fs.stat(dir)?.type != FileType.DIRECTORY) badRequest("$dir is not a directory")
        val max = 1000
        val names = Fs.list(dir)
        val entries =
            names
                .asSequence()
                .mapNotNull { name ->
                    val full = if (dir == "/") "/$name" else "$dir/$name"
                    val entry = Fs.lstat(full) ?: return@mapNotNull null
                    val target = if (entry.type == FileType.LINK) Fs.realPath(full) else full
                    val visible =
                        target != null &&
                            (node.policy.allowed(target) || (Fs.stat(target)?.type == FileType.DIRECTORY && node.policy.leadsTo(target)))
                    if (!visible) return@mapNotNull null
                    buildJsonObject {
                        put("name", name)
                        put("type", entry.type.wire)
                        put("size_bytes", entry.size)
                        put("mode", (entry.mode and 0xFFF).toString(8).padStart(4, '0'))
                        put("owner", Fs.userName(entry.uid) ?: entry.uid.toString())
                        put("group", Fs.groupName(entry.gid) ?: entry.gid.toString())
                        put("modified", Parsers.iso(entry.modifiedEpochSeconds))
                        if (entry.type == FileType.LINK) put("target", target)
                    }
                }.take(max + 1)
                .toList()
        return Answer(
            buildJsonObject {
                put("path", dir)
                put("entries", JsonArray(entries.take(max)))
            },
            truncated = entries.size > max,
        )
    }

    fun processes(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer = Answer(Platform.processes(node, byMemory = args.string("sort") == "memory", limit = args.int("limit") ?: 20))

    fun ports(): Answer = Answer(Platform.ports())

    fun history(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val count = args.int("lines") ?: 50
        if (!Fs.exists(node.config.audit)) return Answer(JsonArray(emptyList()))
        val slice = Fs.tail(node.config.audit, count, maxBytes = count.toLong() * 8192)
        // The log keeps every argument as it came, deploy ones included; what leaves the node is redacted.
        return Answer(
            JsonArray(slice.lines.mapNotNull { runCatching { LenientJson.parseToJsonElement(node.redactor.redact(it)) }.getOrNull() }),
        )
    }

    fun check(
        node: Node,
        args: Map<String, JsonElement>,
    ): Answer {
        val (entry, spec) = Scripts.find(node, ScriptKind.CHECK, args.string("name")!!)
        val env = Scripts.environment(node, spec, args.obj("args"))
        val r = Scripts.run(entry, spec, env)
        if (r.timedOut) throw LimenException(ErrorCode.TIMEOUT, "check ${spec.name} did not finish in ${spec.timeoutSeconds}s")
        val status =
            when (r.exitCode) {
                0 -> "ok"
                1 -> "warn"
                2 -> "fail"
                else -> "unknown"
            }
        val output = node.redactor.redact(r.out).trimEnd()
        return Answer(
            buildJsonObject {
                put("check", spec.name)
                put("status", status)
                put("exit_code", r.exitCode)
                r.signal?.let { put("signal", it) }
                put("summary", output.lineSequence().firstOrNull().orEmpty())
                put("detail", output.substringAfter('\n', ""))
                if (r.err.isNotBlank()) {
                    put(
                        "stderr",
                        node.redactor
                            .redact(r.err)
                            .trimEnd()
                            .takeLast(4000),
                    )
                }
            },
            r.truncated,
        )
    }

    /** [requested] resolved and checked against the policy: the path that is safe to open. */
    fun allowedFile(
        node: Node,
        requested: String,
    ): String {
        val resolved = resolve(node, requested) { node.policy.allowed(it) }
        when (val decision = node.policy.check(resolved)) {
            is PathPolicy.Decision.Denied -> throw LimenException(ErrorCode.DENIED, decision.reason)
            PathPolicy.Decision.Allowed -> Unit
        }
        val info = Fs.stat(resolved)!!
        if (info.type != FileType.FILE) badRequest("$resolved is not a regular file")
        // A window of lines can fall between a key's markers, where redaction can't see it. Keys live in small files,
        // so those are searched whole; in a large one, a log, the redactor only masks keys the window holds entire.
        if (info.size <= KEY_FILE_MAX_BYTES && Fs.contains(resolved, PRIVATE_KEY, KEY_FILE_MAX_BYTES)) {
            throw LimenException(ErrorCode.DENIED, "$resolved holds a private key")
        }
        return resolved
    }

    /**
     * [requested] with symlinks and `..` resolved. A path that doesn't exist is judged by its normalised form: one
     * the policy would refuse is `denied` whether or not it exists, so the answer never tells a denied path's
     * existence.
     */
    private fun resolve(
        node: Node,
        requested: String,
        visible: (String) -> Boolean,
    ): String {
        if (!requested.startsWith("/")) badRequest("$requested is not an absolute path")
        Fs.realPath(requested)?.let { return it }
        val normalised = resolveExisting(PathPolicy.normalize(requested))
        if (!visible(normalised)) throw LimenException(ErrorCode.DENIED, denial(node, normalised))
        throw LimenException(ErrorCode.NOT_FOUND, "$normalised does not exist")
    }

    /** [path] with its longest existing prefix resolved, so a link to a denied directory is judged where it leads. */
    private fun resolveExisting(path: String): String {
        var head = path
        val missing = ArrayDeque<String>()
        while (head != "/") {
            Fs.realPath(head)?.let { real -> return PathPolicy.normalize("$real/${missing.joinToString("/")}") }
            missing.addFirst(head.substringAfterLast('/'))
            head = head.substringBeforeLast('/').ifEmpty { "/" }
        }
        return path
    }

    private fun denial(
        node: Node,
        path: String,
    ): String = (node.policy.check(path) as? PathPolicy.Decision.Denied)?.reason ?: "$path is not readable"

    private const val PRIVATE_KEY = "PRIVATE KEY-----"
    private const val MAX_SCAN_BYTES = 16L * 1024 * 1024
    private const val KEY_FILE_MAX_BYTES = 1024L * 1024

    /** `30m` (that long ago) or `2026-09-26T08:00Z`. */
    private fun instant(
        node: Node,
        text: String,
    ): Instant {
        Durations.parse(text)?.let { return node.now() - it }
        val withSeconds = if (Regex("T\\d{2}:\\d{2}Z$").containsMatchIn(text)) text.dropLast(1) + ":00Z" else text
        return runCatching { Instant.parse(withSeconds) }.getOrElse { badRequest("'$text' is not a time") }
    }

    private fun journalTime(i: Instant) = Parsers.iso(i.epochSeconds).replace('T', ' ').removeSuffix("Z") + " UTC"

    private fun dockerTime(i: Instant) = Parsers.iso(i.epochSeconds)

    /** [text] as a PCRE pattern that matches only itself: every non-alphanumeric character escaped. */
    fun pcreLiteral(text: String): String =
        buildString { text.forEach { if (it.isLetterOrDigit() || it == ' ') append(it) else append('\\').append(it) } }
}
