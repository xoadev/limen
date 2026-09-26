package limen.cli.hub

import io.ktor.http.ContentType
import io.ktor.http.HttpStatusCode
import io.ktor.server.application.ApplicationCall
import io.ktor.server.cio.CIO
import io.ktor.server.engine.embeddedServer
import io.ktor.server.request.receiveText
import io.ktor.server.response.respond
import io.ktor.server.response.respondText
import io.ktor.server.routing.delete
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import io.ktor.server.routing.routing
import kotlinx.coroutines.runBlocking
import limen.cli.os.Sys
import limen.core.config.HubConfig

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
 */
object Http {
    private const val MAX_BODY = 1024 * 1024

    fun run(
        config: HubConfig,
        client: NodeClient,
        token: String,
    ) {
        val server = McpServer(client, notify = null, log = { Sys.err("limen: $it\n") })
        Sys.err("limen: serving MCP on http://${config.listen}/mcp for ${client.nodes.size} node(s)\n")
        embeddedServer(CIO, port = config.listenPort, host = config.listenHost) {
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
            }
        }.start(wait = true)
    }

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
        val given =
            call.request.headers["Authorization"]
                ?.removePrefix("Bearer ")
                ?.trim()
                .orEmpty()
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
