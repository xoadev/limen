package limen.cli.hub

import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import kotlinx.serialization.json.putJsonObject
import limen.core.Args
import limen.core.LIMEN_VERSION
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.NodeResponse
import limen.core.Param
import limen.core.ParamType
import limen.core.PrettyJson
import limen.core.Requests
import limen.core.Role
import limen.core.WireJson
import limen.core.scripts.Catalog
import limen.core.scripts.ScriptSpec
import kotlin.time.TimeSource

/**
 * The MCP server (spec §5, §9): JSON-RPC 2.0, one message in, at most one out. Transport-free: `limen mcp` feeds it
 * lines from stdin and `limen serve` HTTP bodies. Own implementation, no SDK.
 *
 * Every tool is a read request to one node. Actions and setup scripts are listed in `nodes` and never become tools
 * (spec §1, principle 2).
 */
class McpServer(
    private val client: NodeClient,
    /** Sends a notification to the client, where the transport can (stdio). */
    private val notify: ((String) -> Unit)? = null,
    private val log: (String) -> Unit = {},
) {
    /** The last `hello` of each node: its catalog decides the `check_<name>` tools. */
    private var hellos: Map<String, NodeResponse>? = null

    suspend fun handle(line: String): String? {
        val message =
            try {
                LenientJson.parseToJsonElement(line) as? JsonObject ?: return error(JsonNull, PARSE_ERROR, "expected a JSON object")
            } catch (e: Exception) {
                return error(JsonNull, PARSE_ERROR, "invalid JSON")
            }
        val id = message["id"]
        val method = (message["method"] as? JsonPrimitive)?.contentOrNull ?: return id?.let { error(it, INVALID_REQUEST, "no method") }
        val params = message["params"] as? JsonObject ?: JsonObject(emptyMap())
        if (id == null) return null // A notification: nothing to answer, whatever it is.
        return try {
            when (method) {
                "initialize" -> result(id, initialize(params))
                "ping" -> result(id, JsonObject(emptyMap()))
                "tools/list" -> result(id, buildJsonObject { put("tools", JsonArray(tools())) })
                "tools/call" -> result(id, call(params))
                else -> error(id, METHOD_NOT_FOUND, "unknown method $method")
            }
        } catch (e: InvalidParams) {
            error(id, INVALID_PARAMS, e.message ?: "invalid params")
        }
    }

    private fun initialize(params: JsonObject): JsonObject {
        val asked = (params["protocolVersion"] as? JsonPrimitive)?.contentOrNull
        return buildJsonObject {
            put("protocolVersion", if (asked in PROTOCOL_VERSIONS) asked else PROTOCOL_VERSIONS.first())
            putJsonObject("capabilities") { putJsonObject("tools") { put("listChanged", notify != null) } }
            putJsonObject("serverInfo") {
                put("name", "limen")
                put("version", LIMEN_VERSION)
            }
            put("instructions", INSTRUCTIONS)
        }
    }

    private suspend fun tools(): List<JsonObject> {
        val nodes = client.nodes
        val node = nodeParam(nodes)
        val fixed =
            listOf(tool("nodes", NODES_DESCRIPTION, Args.inputSchema(emptyList()))) +
                Requests.all.filter { it.tool && it.role == Role.READ }.map { def ->
                    tool(def.name, def.description, Args.inputSchema(def.params, listOf(node to true)))
                }
        val checks =
            checkTools().map { (name, pair) ->
                val (spec, on) = pair
                tool(
                    "check_$name",
                    "Check script `$name`: ${spec.description}. Answers ok, warn, fail or unknown with a summary.",
                    Args.inputSchema(spec.params, listOf(nodeParam(on) to true)),
                )
            }
        return fixed + checks
    }

    private fun tool(
        name: String,
        description: String,
        schema: JsonObject,
    ) = buildJsonObject {
        put("name", name)
        put("description", description)
        put("inputSchema", schema)
        putJsonObject("annotations") {
            put("readOnlyHint", true)
            put("destructiveHint", false)
        }
    }

    private fun nodeParam(nodes: List<String>) =
        if (nodes.isEmpty()) {
            Param("node", ParamType.STRING, "Which machine. There are none yet: the hub adds them with `limen invite`")
        } else {
            Param("node", ParamType.ENUM, "Which machine", values = nodes)
        }

    /** Check name → its spec and the nodes that have it. A name declared with different arguments is left out. */
    private suspend fun checkTools(): Map<String, Pair<ScriptSpec, List<String>>> {
        val byName = linkedMapOf<String, MutableList<Pair<String, ScriptSpec>>>()
        for ((node, catalog) in catalogs()) {
            catalog.checks.forEach { byName.getOrPut(it.name) { mutableListOf() } += node to it }
        }
        return byName
            .filterValues { list -> list.map { it.second.params }.distinct().size == 1 }
            .mapValues { (_, list) -> list.first().second to list.map { it.first } }
    }

    private suspend fun catalogs(): Map<String, Catalog> {
        val hellos = hellos?.takeIf { it.keys == client.nodes.toSet() } ?: refresh()
        return hellos
            .mapNotNull { (node, r) ->
                val data = r.data as? JsonObject ?: return@mapNotNull null
                val catalog = data["catalog"] ?: return@mapNotNull null
                runCatching { node to LenientJson.decodeFromJsonElement(Catalog.serializer(), catalog) }.getOrNull()
            }.toMap()
    }

    private suspend fun refresh(): Map<String, NodeResponse> {
        val before = hellos?.let { checkSignature() }
        val fresh =
            coroutineScope {
                client.nodes
                    .map { node ->
                        async {
                            node to
                                try {
                                    client.call(node, "hello", JsonObject(emptyMap()))
                                } catch (e: LimenException) {
                                    NodeResponse.failure(e)
                                }
                        }
                    }.awaitAll()
                    .toMap()
            }
        hellos = fresh
        if (before != null && before != checkSignature()) {
            notify?.invoke(WireJson.encodeToString(JsonObject.serializer(), notification("notifications/tools/list_changed")))
        }
        return fresh
    }

    private suspend fun checkSignature() = checkTools().mapValues { it.value.first to it.value.second.sorted() }

    private suspend fun call(params: JsonObject): JsonObject {
        val name = (params["name"] as? JsonPrimitive)?.contentOrNull ?: throw InvalidParams("tools/call needs a name")
        val arguments = params["arguments"] as? JsonObject ?: JsonObject(emptyMap())
        if (name == "nodes") return nodes()
        val known =
            Requests.find(name)?.let { it.tool && it.role == Role.READ } == true ||
                (name.startsWith("check_") && name.removePrefix("check_") in checkTools())
        if (!known) throw InvalidParams("unknown tool $name")
        val node = (arguments["node"] as? JsonPrimitive)?.contentOrNull ?: return toolError("missing argument 'node'")
        if (node !in client.nodes) return toolError("no node named '$node'; the nodes are ${client.nodes.joinToString(", ")}")
        val rest = JsonObject(arguments - "node")
        val (request, args) =
            when {
                name.startsWith("check_") -> {
                    val check = name.removePrefix("check_")
                    val spec = checkTools()[check]?.first ?: throw InvalidParams("unknown tool $name")
                    try {
                        Args.validate(spec.params, rest)
                    } catch (e: LimenException) {
                        return toolError(e.message ?: "bad arguments")
                    }
                    "check" to
                        buildJsonObject {
                            put("name", check)
                            put("args", rest)
                        }
                }

                else -> {
                    val def = Requests.find(name)?.takeIf { it.tool && it.role == Role.READ } ?: throw InvalidParams("unknown tool $name")
                    try {
                        Args.validate(def.params, rest)
                    } catch (e: LimenException) {
                        return toolError(e.message ?: "bad arguments")
                    }
                    name to rest
                }
            }
        val started = TimeSource.Monotonic.markNow()
        val response =
            try {
                client.call(node, request, args)
            } catch (e: LimenException) {
                NodeResponse.failure(e)
            }
        log("tool=$name node=$node result=${response.error?.code ?: "ok"} ${started.elapsedNow().inWholeMilliseconds}ms")
        return render(response)
    }

    private suspend fun nodes(): JsonObject {
        val fresh = refresh()
        val catalogs = catalogs()
        val conflicts =
            catalogs.values
                .flatMap { it.checks }
                .groupBy { it.name }
                .filterValues { specs ->
                    specs.map { it.params }.distinct().size > 1
                }.keys
        val summary =
            buildJsonArray {
                for (node in client.nodes) {
                    val r = fresh[node]
                    val data = r?.data as? JsonObject
                    add(
                        buildJsonObject {
                            put("node", node)
                            put("reachable", r?.ok == true)
                            r?.error?.let { put("error", "${it.code}: ${it.message}") }
                            if (data != null) {
                                listOf("version", "hostname", "os", "kernel", "arch", "docker").forEach { k -> data[k]?.let { put(k, it) } }
                            }
                            catalogs[node]?.let { c ->
                                putJsonArray("checks") { c.checks.forEach { add(JsonPrimitive("${it.name}: ${it.description}")) } }
                                putJsonArray("actions") { c.actions.forEach { add(JsonPrimitive("${it.name}: ${it.description}")) } }
                                putJsonArray("setup") { c.setup.forEach { add(JsonPrimitive("${it.name}: ${it.description}")) } }
                                if (c.problems.isNotEmpty()) {
                                    putJsonArray(
                                        "script_problems",
                                    ) { c.problems.forEach { add(JsonPrimitive(it)) } }
                                }
                            }
                        },
                    )
                }
            }
        return text(
            PrettyJson.encodeToString(
                JsonObject.serializer(),
                buildJsonObject {
                    put("nodes", summary)
                    if (conflicts.isNotEmpty()) {
                        put(
                            "check_conflicts",
                            JsonArray(conflicts.map { JsonPrimitive("$it: declared with different arguments on different nodes") }),
                        )
                    }
                    put("note", "Actions and setup scripts are listed for reference; limen never runs them through MCP.")
                },
            ),
        )
    }

    private fun render(response: NodeResponse): JsonObject {
        if (!response.ok) {
            val e = response.error
            val versions = e?.versions?.let { " (the node speaks versions ${it.joinToString()}; update limen there)" }.orEmpty()
            return toolError("${e?.code ?: "internal"}: ${e?.message ?: "no answer"}$versions")
        }
        val body = PrettyJson.encodeToString(JsonElement.serializer(), response.data ?: JsonNull)
        return text(if (response.truncated) "$body\n\n[truncated: a limit cut this answer; narrow the request]" else body)
    }

    private fun text(
        text: String,
        isError: Boolean = false,
    ) = buildJsonObject {
        putJsonArray("content") {
            add(
                buildJsonObject {
                    put("type", "text")
                    put("text", text)
                },
            )
        }
        put("isError", isError)
    }

    private fun toolError(message: String) = text(message, isError = true)

    private class InvalidParams(
        message: String,
    ) : Exception(message)

    private fun result(
        id: JsonElement,
        result: JsonObject,
    ): String =
        WireJson.encodeToString(
            JsonObject.serializer(),
            buildJsonObject {
                put("jsonrpc", "2.0")
                put("id", id)
                put("result", result)
            },
        )

    private fun error(
        id: JsonElement,
        code: Int,
        message: String,
    ): String =
        WireJson.encodeToString(
            JsonObject.serializer(),
            buildJsonObject {
                put("jsonrpc", "2.0")
                put("id", id)
                putJsonObject("error") {
                    put("code", code)
                    put("message", message)
                }
            },
        )

    private fun notification(method: String) =
        buildJsonObject {
            put("jsonrpc", "2.0")
            put("method", method)
        }

    companion object {
        /** Newest first; the first is what a client that asks for something else gets. */
        val PROTOCOL_VERSIONS = listOf("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")

        const val PARSE_ERROR = -32700
        const val INVALID_REQUEST = -32600
        const val METHOD_NOT_FOUND = -32601
        const val INVALID_PARAMS = -32602

        private const val NODES_DESCRIPTION =
            "The machines this server can inspect: whether each answers, its OS and limen version, and the scripts it " +
                "has (checks you can run as check_<name> tools; actions and setup scripts for reference only)."

        const val INSTRUCTIONS =
            "Read-only access to Linux machines through limen. Every tool takes a `node`; call `nodes` first to see " +
                "them. Start a diagnosis with `status`, then `services`/`service`, `containers`/`container` and `logs`. " +
                "Files are readable only where the node allows it; a `denied` answer is the node's decision, not an " +
                "error to work around. Nothing here can change a machine: to fix something, say what should be run " +
                "and let a person run it."
    }
}

private operator fun JsonObject.minus(key: String): Map<String, JsonElement> = filterKeys { it != key }
