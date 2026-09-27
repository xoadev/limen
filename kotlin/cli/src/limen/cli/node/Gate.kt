package limen.cli.node

import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import limen.cli.os.Fs
import limen.cli.os.Sys
import limen.core.Args
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.NodeRequest
import limen.core.NodeResponse
import limen.core.PROTOCOL_VERSIONS
import limen.core.Requests
import limen.core.Role
import limen.core.WireJson
import limen.core.badRequest
import limen.core.config.NodeConfig
import kotlin.time.TimeSource

/**
 * `limen gate --role <role>`: the forced command (spec §3, §4). Reads one JSON request from stdin, answers it within
 * the role and exits. A read request always gets a JSON answer on stdout, even when the configuration is broken;
 * a deploy request streams its scripts' output as text and exits with their result.
 */
object Gate {
    private const val MAX_REQUEST = 1024 * 1024

    fun run(
        role: Role,
        configPath: String = NodeConfig.PATH,
    ): Int {
        Sys.chdirRoot()
        val started = TimeSource.Monotonic.markNow()
        var request: NodeRequest? = null
        var node: Node? = null
        var code = "ok"
        try {
            request = parse(Sys.readStdin(MAX_REQUEST))
            node = Node.load(configPath)
            val def = Requests.find(request.request) ?: badRequest("unknown request '${request.request}'")
            if (def.role != role) throw LimenException(ErrorCode.DENIED, "'${def.name}' is not allowed for the ${role.wire} role")
            val args = Args.validate(def.params, request.args)
            if (role == Role.DEPLOY) {
                val ok = Deploy.run(node, def.name, args)
                if (!ok) code = "failed"
                return if (ok) 0 else 1
            }
            val answer = answer(node, def.name, args)
            write(node, NodeResponse.success(answer.data, answer.truncated))
            return 0
        } catch (e: LimenException) {
            code = e.code.wire
            return fail(role, node, e)
        } catch (e: Exception) {
            code = ErrorCode.INTERNAL.wire
            return fail(role, node, LimenException(ErrorCode.INTERNAL, e.message ?: e::class.simpleName ?: "error"))
        } finally {
            node?.let { Audit.record(it, role, request, code, started.elapsedNow().inWholeMilliseconds) }
        }
    }

    fun answer(
        node: Node,
        name: String,
        args: Map<String, JsonElement>,
    ): Answer =
        when (name) {
            "hello" -> Read.hello(node)
            "status" -> Read.status(node)
            "services" -> Read.services(node, args)
            "service" -> Read.service(node, args)
            "containers" -> Read.containers(node, args)
            "container" -> Read.container(node, args)
            "logs" -> Read.logs(node, args)
            "read_file" -> Read.readFile(node, args)
            "list_dir" -> Read.listDir(node, args)
            "processes" -> Read.processes(node, args)
            "ports" -> Read.ports()
            "history" -> Read.history(node, args)
            "state" -> State.answer(node)
            "check" -> Read.check(node, args)
            else -> badRequest("unknown request '$name'")
        }

    private fun parse(bytes: ByteArray?): NodeRequest {
        bytes ?: badRequest("request larger than $MAX_REQUEST bytes")
        val text = bytes.decodeToString().trim()
        if (text.isEmpty()) badRequest("no request on stdin; send one JSON object, e.g. {\"v\":1,\"request\":\"status\"}")
        val request =
            try {
                WireJson.decodeFromString(NodeRequest.serializer(), text)
            } catch (e: SerializationException) {
                badRequest("malformed request: ${e.message?.lineSequence()?.firstOrNull()}")
            } catch (e: IllegalArgumentException) {
                badRequest("malformed request: ${e.message?.lineSequence()?.firstOrNull()}")
            }
        if (request.v !in PROTOCOL_VERSIONS) {
            throw LimenException(
                ErrorCode.UNSUPPORTED_VERSION,
                "protocol version ${request.v} is not supported by limen on this node",
                PROTOCOL_VERSIONS,
            )
        }
        return request
    }

    private fun write(
        node: Node?,
        response: NodeResponse,
    ) {
        var text = WireJson.encodeToString(NodeResponse.serializer(), response)
        val limit = node?.config?.maxResponseBytes ?: NodeConfig().maxResponseBytes
        val size = text.encodeToByteArray().size
        if (size > limit) {
            text =
                WireJson.encodeToString(
                    NodeResponse.serializer(),
                    NodeResponse.failure(
                        LimenException(
                            ErrorCode.BAD_REQUEST,
                            "the answer is $size bytes, over limits.max_response ($limit); narrow the request",
                        ),
                    ),
                )
        }
        Sys.out(text + "\n")
    }

    private fun fail(
        role: Role,
        node: Node?,
        e: LimenException,
    ): Int {
        if (role == Role.DEPLOY) {
            Sys.err("limen: ${e.code.wire}: ${e.message}\n")
            return 1
        }
        write(node, NodeResponse.failure(e))
        return 0
    }
}

/** The node's audit log (spec §8): one JSON line per request, whatever its outcome. */
object Audit {
    fun record(
        node: Node,
        role: Role,
        request: NodeRequest?,
        code: String,
        durationMs: Long,
    ) {
        val entry =
            buildJsonObject {
                put(
                    "time",
                    limen.core.system.Parsers
                        .iso(node.now().epochSeconds),
                )
                put("role", role.wire)
                put("request", request?.request)
                put("args", request?.args ?: JsonObject(emptyMap()))
                put("client", Sys.env("SSH_CONNECTION")?.substringBefore(' '))
                put("user", Sys.env("SUDO_USER"))
                put("result", code)
                put("duration_ms", durationMs)
            }
        try {
            Fs.mkdirs(node.config.audit.substringBeforeLast('/'), 0b111_000_000)
            // One old file kept, no logrotate needed: OpenWrt has none, and its /var/log lives in RAM.
            if ((Fs.stat(node.config.audit)?.size ?: 0) > node.config.auditMaxBytes) {
                platform.posix.rename(node.config.audit, node.config.audit + ".1")
            }
            Fs.appendLine(node.config.audit, WireJson.encodeToString(JsonObject.serializer(), entry))
        } catch (e: Exception) {
            Sys.err("limen: cannot write the audit log ${node.config.audit}: ${e.message}\n")
        }
    }
}
