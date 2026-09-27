package limen.cli

import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.boolean
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import limen.cli.hub.Http
import limen.cli.hub.McpServer
import limen.cli.hub.NodeClient
import limen.core.LenientJson
import limen.core.NodeError
import limen.core.NodeResponse
import limen.core.Param
import limen.core.ParamType
import limen.core.WireJson
import limen.core.scripts.Catalog
import limen.core.scripts.ScriptKind
import limen.core.scripts.ScriptSpec
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

class McpServerTest {
    private val threshold = Param("threshold", ParamType.INT, default = JsonPrimitive(90), min = 1, max = 100)

    /** A node client that answers from memory and remembers what it was asked. */
    private class FakeClient(
        val catalogs: MutableMap<String, Catalog>,
    ) : NodeClient {
        val calls = mutableListOf<Triple<String, String, JsonObject>>()
        override val nodes = catalogs.keys.toList()

        override suspend fun call(
            node: String,
            request: String,
            args: JsonObject,
        ): NodeResponse {
            calls += Triple(node, request, args)
            return when (request) {
                "hello" -> {
                    NodeResponse.success(
                        buildJsonObject {
                            put("version", "dev")
                            put("catalog", WireJson.encodeToJsonElement(Catalog.serializer(), catalogs.getValue(node)))
                        },
                    )
                }

                "read_file" -> {
                    NodeResponse(ok = false, error = NodeError("denied", "/etc/shadow is never readable"))
                }

                else -> {
                    NodeResponse.success(buildJsonObject { put("asked", request) }, truncated = request == "logs")
                }
            }
        }
    }

    private fun check(
        name: String,
        vararg params: Param,
    ) = ScriptSpec(name, ScriptKind.CHECK, "About $name", 60, params.toList())

    private fun client() =
        FakeClient(
            linkedMapOf(
                "nas" to
                    Catalog(
                        checks = listOf(check("disk", threshold), check("backups")),
                        actions = listOf(ScriptSpec("restart-immich", ScriptKind.ACTION, "Restarts Immich", 60)),
                    ),
                "router" to Catalog(checks = listOf(check("disk", threshold), check("backups", threshold))),
            ),
        )

    private fun rpc(
        server: McpServer,
        method: String,
        params: JsonObject = JsonObject(emptyMap()),
    ): JsonObject {
        val line =
            WireJson.encodeToString(
                JsonObject.serializer(),
                buildJsonObject {
                    put("jsonrpc", "2.0")
                    put("id", 7)
                    put("method", method)
                    put("params", params)
                },
            )
        return LenientJson.parseToJsonElement(runBlocking { server.handle(line) }!!).jsonObject
    }

    private fun callTool(
        server: McpServer,
        name: String,
        args: JsonObject,
    ): JsonObject =
        rpc(
            server,
            "tools/call",
            buildJsonObject {
                put("name", name)
                put("arguments", args)
            },
        )["result"]!!.jsonObject

    private fun JsonObject.text() =
        this["content"]!!
            .jsonArray[0]
            .jsonObject["text"]!!
            .jsonPrimitive.content

    private fun JsonObject.isError() = this["isError"]!!.jsonPrimitive.boolean

    @Test
    fun initializeNegotiatesTheVersion() {
        val server = McpServer(client())
        val init = rpc(server, "initialize", buildJsonObject { put("protocolVersion", "2025-06-18") })["result"]!!.jsonObject
        assertEquals("2025-06-18", init["protocolVersion"]!!.jsonPrimitive.content)
        assertEquals("limen", init["serverInfo"]!!.jsonObject["name"]!!.jsonPrimitive.content)
        val other = rpc(server, "initialize", buildJsonObject { put("protocolVersion", "1999-01-01") })["result"]!!.jsonObject
        assertEquals(McpServer.PROTOCOL_VERSIONS.first(), other["protocolVersion"]!!.jsonPrimitive.content)
    }

    @Test
    fun notificationsGetNoAnswer() {
        assertNull(runBlocking { McpServer(client()).handle("""{"jsonrpc":"2.0","method":"notifications/initialized"}""") })
    }

    @Test
    fun toolsAreReadRequestsAndConsistentChecks() {
        val tools = rpc(McpServer(client()), "tools/list")["result"]!!.jsonObject["tools"]!!.jsonArray.map { it.jsonObject }
        val names = tools.map { it["name"]!!.jsonPrimitive.content }
        assertTrue(names.containsAll(listOf("nodes", "status", "logs", "read_file", "list_dir", "check_disk")), names.toString())
        // Nothing that changes a machine, and nothing internal.
        assertFalse(names.any { it in listOf("apply", "action", "hello", "check") || it.startsWith("action") }, names.toString())
        // `backups` has different arguments on each node: no tool until that is fixed.
        assertFalse("check_backups" in names)
        assertTrue(tools.all { it["annotations"]!!.jsonObject["readOnlyHint"]!!.jsonPrimitive.boolean })
        val disk = tools.first { it["name"]!!.jsonPrimitive.content == "check_disk" }
        val node = disk["inputSchema"]!!.jsonObject["properties"]!!.jsonObject["node"]!!.jsonObject
        assertEquals("""["nas","router"]""", node["enum"].toString())
    }

    @Test
    fun callsGoToTheNodeWithoutTheNodeArgument() {
        val client = client()
        val server = McpServer(client)
        val result =
            callTool(
                server,
                "logs",
                buildJsonObject {
                    put("node", "nas")
                    put("source", "unit")
                    put("name", "nginx")
                },
            )
        assertFalse(result.isError())
        assertTrue("[truncated" in result.text())
        val (node, request, args) = client.calls.last()
        assertEquals("nas" to "logs", node to request)
        assertEquals(setOf("source", "name"), args.keys)
    }

    @Test
    fun checksBecomeTheCheckRequest() {
        val client = client()
        val server = McpServer(client)
        rpc(server, "tools/list")
        callTool(
            server,
            "check_disk",
            buildJsonObject {
                put("node", "router")
                put("threshold", 80)
            },
        )
        val (_, request, args) = client.calls.last()
        assertEquals("check", request)
        assertEquals("""{"name":"disk","args":{"threshold":80}}""", args.toString())
    }

    @Test
    fun badArgumentsAndNodeErrorsAreToolErrors() {
        val client = client()
        val server = McpServer(client)
        val before = client.calls.size
        val missing = callTool(server, "status", JsonObject(emptyMap()))
        assertTrue(missing.isError())
        val wrongNode = callTool(server, "status", buildJsonObject { put("node", "olympus") })
        assertTrue("no node named 'olympus'" in wrongNode.text())
        val badArg =
            callTool(
                server,
                "service",
                buildJsonObject {
                    put("node", "nas")
                    put("name", "x; reboot")
                },
            )
        assertTrue(badArg.isError())
        assertEquals(before, client.calls.size, "nothing invalid reaches a node")
        val denied =
            callTool(
                server,
                "read_file",
                buildJsonObject {
                    put("node", "nas")
                    put("path", "/etc/shadow")
                },
            )
        assertTrue(denied.isError())
        assertEquals("denied: /etc/shadow is never readable", denied.text())
    }

    @Test
    fun nodesListsActionsButNeverAsTools() {
        val text = callTool(McpServer(client()), "nodes", JsonObject(emptyMap())).text()
        assertTrue("restart-immich: Restarts Immich" in text, text)
        assertTrue("backups: declared with different arguments" in text, text)
    }

    @Test
    fun aChangedCatalogIsAnnounced() {
        val client = client()
        val sent = mutableListOf<String>()
        val server = McpServer(client, notify = { sent += it })
        rpc(server, "tools/list")
        callTool(server, "nodes", JsonObject(emptyMap()))
        assertTrue(sent.isEmpty())
        client.catalogs["nas"] = Catalog(checks = listOf(check("disk", threshold), check("certs")))
        callTool(server, "nodes", JsonObject(emptyMap()))
        assertEquals(1, sent.size)
        assertTrue("notifications/tools/list_changed" in sent[0])
    }

    @Test
    fun unknownMethodsAndToolsAreProtocolErrors() {
        val server = McpServer(client())
        assertEquals(
            -32601,
            rpc(server, "resources/list")["error"]!!
                .jsonObject["code"]!!
                .jsonPrimitive.content
                .toInt(),
        )
        val unknown = rpc(server, "tools/call", buildJsonObject { put("name", "rm") })
        assertEquals(
            -32602,
            unknown["error"]!!
                .jsonObject["code"]!!
                .jsonPrimitive.content
                .toInt(),
        )
    }

    @Test
    fun parseArgsTypesByTheSchema() {
        val params = listOf(Param("name", ParamType.STRING), Param("lines", ParamType.INT))
        assertEquals("""{"name":"123","lines":5,"all":true}""", parseArgs(listOf("name=123", "lines=5", "all=true"), params).toString())
    }

    @Test
    fun tokenComparison() {
        assertTrue(Http.constantTimeEquals("abcdefghijklmnop", "abcdefghijklmnop"))
        assertFalse(Http.constantTimeEquals("abcdefghijklmnop", "abcdefghijklmnoq"))
        assertFalse(Http.constantTimeEquals("abc", "abcd"))
        assertFalse(Http.constantTimeEquals("", "x"))
    }
}
