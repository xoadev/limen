package limen.cli.hub

import io.ktor.http.ContentType
import io.ktor.http.HttpStatusCode
import io.ktor.server.application.ApplicationCall
import io.ktor.server.cio.CIO
import io.ktor.server.engine.embeddedServer
import io.ktor.server.plugins.origin
import io.ktor.server.request.receiveText
import io.ktor.server.response.respond
import io.ktor.server.response.respondText
import io.ktor.server.routing.delete
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import io.ktor.server.routing.routing
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.WireJson
import limen.core.config.HubConfig
import limen.core.join.Arrival
import limen.core.join.Invitation
import limen.core.join.Keys
import limen.core.join.Welcome

/** `limen mcp`: newline-delimited JSON-RPC on stdin and stdout (spec §9). Logs go to stderr: stdout is the protocol. */
object Stdio {
    fun run(client: NodeClient) {
        val server = McpServer(client, notify = { Sys.out(it + "\n") }, log = { Sys.err("limen: $it\n") })
        while (true) {
            val line = readlnOrNull() ?: return
            if (line.isBlank()) continue
            val answer = runBlocking { server.handle(line) }
            if (answer != null) Sys.out(answer + "\n")
        }
    }
}

/**
 * `limen serve`: MCP Streamable HTTP on `POST /mcp`, JSON answers and no SSE (spec §9). A bearer token is required
 * and `Origin`, when a browser sends one, must be listed, against DNS rebinding.
 *
 * And `/join/<code>` (spec §10.1), where a node with an invitation fetches the hub's key and then reports its own.
 * No token there: the one-time code is the authorisation, and all it allows is adding the node it names.
 */
object Http {
    private const val MAX_BODY = 1024 * 1024

    fun run(
        hub: Hub,
        live: LiveHub,
        listen: String,
        token: String,
    ) {
        val config = live.config()
        val server = McpServer(live, notify = null, log = { Sys.err("limen: $it\n") })
        val host = listen.substringBeforeLast(':')
        val port = listen.substringAfterLast(':').toInt()
        Sys.err("limen: serving MCP on http://$listen/mcp; ${live.nodes.size} node(s); hub key ${Keys.fingerprint(hub.publicKey)}\n")
        Sys.err("limen: `limen connect` prints the line for an MCP client, `limen invite <name>` the one for a new node\n")
        embeddedServer(CIO, port = port, host = host) {
            routing {
                post("/mcp") {
                    if (!allowed(call, config, token)) return@post
                    val length = call.request.headers["Content-Length"]?.toLongOrNull()
                    if (length == null || length > MAX_BODY) {
                        call.respond(HttpStatusCode.PayloadTooLarge)
                        return@post
                    }
                    val answer = server.handle(call.receiveText())
                    if (answer == null) {
                        call.respond(HttpStatusCode.Accepted)
                    } else {
                        call.respondText(answer, ContentType.Application.Json)
                    }
                }
                // No SSE stream and no sessions to end: every call is a request and its answer.
                get("/mcp") { call.respond(HttpStatusCode.MethodNotAllowed) }
                delete("/mcp") { call.respond(HttpStatusCode.MethodNotAllowed) }

                get("/join/{code}") {
                    val invitation = hub.invitation(call.parameters["code"].orEmpty())
                    if (invitation == null) {
                        call.respondText(
                            joinError("this invitation does not exist, was used, or expired"),
                            ContentType.Application.Json,
                            HttpStatusCode.NotFound,
                        )
                    } else {
                        call.respondText(WireJson.encodeToString(Invitation.serializer(), invitation), ContentType.Application.Json)
                    }
                }
                post("/join/{code}") {
                    val length = call.request.headers["Content-Length"]?.toLongOrNull()
                    if (length == null || length > 16 * 1024) {
                        call.respond(HttpStatusCode.PayloadTooLarge)
                        return@post
                    }
                    val code = call.parameters["code"].orEmpty()
                    try {
                        val arrival = LenientJson.decodeFromString(Arrival.serializer(), call.receiveText())
                        val welcome = hub.arrive(code, arrival, call.request.origin.remoteHost, live)
                        Sys.err("limen: node ${welcome.name} joined from ${welcome.address}: ${welcome.detail}\n")
                        call.respondText(WireJson.encodeToString(Welcome.serializer(), welcome), ContentType.Application.Json)
                    } catch (e: LimenException) {
                        val status = if (e.code == ErrorCode.NOT_FOUND) HttpStatusCode.NotFound else HttpStatusCode.BadRequest
                        call.respondText(joinError(e.message ?: "rejected"), ContentType.Application.Json, status)
                    } catch (e: Exception) {
                        call.respondText(joinError("malformed request"), ContentType.Application.Json, HttpStatusCode.BadRequest)
                    }
                }
            }
        }.start(wait = true)
    }

    private fun joinError(message: String) = WireJson.encodeToString(JsonObject.serializer(), buildJsonObject { put("error", message) })

    private suspend fun allowed(
        call: ApplicationCall,
        config: HubConfig,
        token: String,
    ): Boolean {
        val origin = call.request.headers["Origin"]
        if (origin != null && origin !in config.origins) {
            call.respond(HttpStatusCode.Forbidden)
            return false
        }
        val header = call.request.headers["Authorization"].orEmpty()
        val given = if (header.startsWith("Bearer ", ignoreCase = true)) header.substring("Bearer ".length).trim() else ""
        if (!constantTimeEquals(given, token)) {
            call.response.headers.append("WWW-Authenticate", "Bearer")
            call.respond(HttpStatusCode.Unauthorized)
            return false
        }
        return true
    }

    /** Compares every byte whatever the first difference, so the time taken does not reveal the token. */
    fun constantTimeEquals(
        a: String,
        b: String,
    ): Boolean {
        val x = a.encodeToByteArray()
        val y = b.encodeToByteArray()
        var diff = x.size xor y.size
        for (i in 0 until maxOf(x.size, y.size)) {
            diff = diff or ((x.getOrElse(i) { 0 }.toInt()) xor (y.getOrElse(i) { 0 }.toInt()))
        }
        return diff == 0
    }
}
