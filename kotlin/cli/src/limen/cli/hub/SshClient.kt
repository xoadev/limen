package limen.cli.hub

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.IO
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.JsonObject
import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.ProcResult
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.NodeError
import limen.core.NodeRequest
import limen.core.NodeResponse
import limen.core.WireJson
import limen.core.config.HubConfig
import limen.core.config.NodeEntry
import kotlin.time.Duration

/** How the hub asks a node something. The MCP server only sees this; tests give it a fake. */
interface NodeClient {
    val nodes: List<String>

    suspend fun call(
        node: String,
        request: String,
        args: JsonObject,
    ): NodeResponse
}

/**
 * [NodeClient] over the system `ssh` (spec §7.2, §12): batch mode, the configured key only, a `known_hosts` limen
 * writes from the pinned host keys, and connection multiplexing so a tool call does not pay a handshake.
 */
class SshClient(
    private val config: HubConfig,
    private val home: String,
) : NodeClient {
    override val nodes: List<String> = config.nodes.map { it.name }

    private val ssh = Proc.which("ssh") ?: throw LimenException(ErrorCode.UNAVAILABLE, "ssh is not installed on the hub")
    private val runtime = runtimeDir()
    private val knownHosts = "$runtime/known_hosts"
    private val identity = if (config.identity.startsWith("/")) config.identity else "$home/${config.identity}"
    private val limits = config.nodes.associate { it.name to Semaphore(config.perNodeConcurrency) }

    init {
        if (Fs.stat(identity)?.type != FileType.FILE) {
            throw LimenException(ErrorCode.UNAVAILABLE, "no SSH key at $identity ([ssh].identity in $home/limen.toml)")
        }
        Fs.writeAtomic(knownHosts, knownHostsText(config.nodes).encodeToByteArray(), 0b110_000_000)
    }

    override suspend fun call(
        node: String,
        request: String,
        args: JsonObject,
    ): NodeResponse {
        val entry = config.node(node) ?: throw LimenException(ErrorCode.BAD_REQUEST, "no node named '$node'")
        val line = WireJson.encodeToString(NodeRequest.serializer(), NodeRequest(1, request, args)) + "\n"
        val result =
            limits.getValue(node).withPermit {
                withContext(Dispatchers.IO) { exchange(entry, entry.user, identity, line.encodeToByteArray(), config.requestTimeout) }
            }
        return response(node, result)
    }

    /**
     * Sends [line] and hands over the answer as it comes: for `limen call` of a deploy request, whose answer is the
     * scripts' output as text. Returns ssh's exit code.
     */
    fun stream(
        node: String,
        user: String,
        key: String?,
        line: String,
        timeout: Duration,
        onChunk: (Int, ByteArray) -> Unit,
    ): ProcResult {
        val entry = config.node(node) ?: throw LimenException(ErrorCode.BAD_REQUEST, "no node named '$node'")
        // Never through the multiplexed connection: on OpenWrt both roles log in as root, and a deploy request would
        // ride the socket the read key opened, landing in the read role.
        val argv = argv(entry, user, key ?: identity, multiplex = false)
        return Proc.run(argv, env(), line.encodeToByteArray(), timeout, onChunk = onChunk)
    }

    private fun exchange(
        entry: NodeEntry,
        user: String,
        key: String,
        input: ByteArray,
        timeout: Duration,
    ): ProcResult = Proc.run(argv(entry, user, key), env(), input, timeout, maxOutput = 32 * 1024 * 1024)

    private fun argv(
        entry: NodeEntry,
        user: String,
        key: String,
        multiplex: Boolean = true,
    ): List<String> {
        val options =
            listOf(
                "BatchMode=yes",
                "IdentitiesOnly=yes",
                "StrictHostKeyChecking=yes",
                "UserKnownHostsFile=$knownHosts",
                "GlobalKnownHostsFile=/dev/null",
                "HostKeyAlias=${alias(entry)}",
                "ConnectTimeout=${config.connectTimeout.inWholeSeconds.coerceAtLeast(1)}",
                "ServerAliveInterval=15",
                "LogLevel=ERROR",
            ) +
                if (multiplex) {
                    listOf("ControlMaster=auto", "ControlPath=$runtime/cm-%C", "ControlPersist=60")
                } else {
                    listOf("ControlMaster=no", "ControlPath=none")
                }
        return listOf(ssh, "-T", "-i", key) + options.flatMap { listOf("-o", it) } +
            listOf("-p", entry.port.toString(), "-l", user, "--", entry.host)
    }

    private fun env(): List<String> =
        listOfNotNull(
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "LANG=C.UTF-8",
            Sys.env("HOME")?.let { "HOME=$it" },
            Sys.env("USER")?.let { "USER=$it" },
            Sys.env("SSH_AUTH_SOCK")?.let { "SSH_AUTH_SOCK=$it" },
        )

    private fun response(
        node: String,
        r: ProcResult,
    ): NodeResponse {
        if (r.timedOut) return failure(ErrorCode.TIMEOUT, "$node did not answer in ${config.requestTimeout}")
        val out = r.out.trim()
        if (r.exitCode == 255 || (out.isEmpty() && r.exitCode != 0)) {
            val err = r.err.trim()
            return when {
                "Host key verification failed" in err || "REMOTE HOST IDENTIFICATION HAS CHANGED" in err -> {
                    failure(ErrorCode.HOST_KEY_MISMATCH, "$node's host key does not match host_key in limen.toml")
                }

                else -> {
                    failure(ErrorCode.UNREACHABLE, "$node: ${err.lines().lastOrNull()?.ifEmpty { null } ?: "ssh exit ${r.exitCode}"}")
                }
            }
        }
        return try {
            LenientJson.decodeFromString(NodeResponse.serializer(), out.lines().last())
        } catch (e: SerializationException) {
            failure(ErrorCode.INTERNAL, "$node answered something that is not limen's protocol: ${(out + r.err).take(300)}")
        } catch (e: IllegalArgumentException) {
            failure(ErrorCode.INTERNAL, "$node answered something that is not limen's protocol: ${(out + r.err).take(300)}")
        }
    }

    private fun failure(
        code: ErrorCode,
        message: String,
    ) = NodeResponse(ok = false, error = NodeError(code.wire, message))

    companion object {
        /** The name a node's key is filed under (`HostKeyAlias`): its limen name, whatever address it has today. */
        fun alias(entry: NodeEntry) = "limen-${entry.name}"

        fun knownHostsText(nodes: List<NodeEntry>) = nodes.joinToString("") { "${alias(it)} ${it.hostKey}\n" }

        /**
         * `/tmp/limen-<uid>`, for the control sockets and `known_hosts`: short, because a Unix socket path has a
         * limit of 108 bytes, and refused if someone else owns it.
         */
        private fun runtimeDir(): String {
            val base = Sys.env("XDG_RUNTIME_DIR")?.takeIf { it.isNotEmpty() && it.length < 40 } ?: "/tmp"
            val dir = "$base/limen-${Sys.euid()}"
            Fs.mkdirs(dir, 0b111_000_000)
            val info = Fs.lstat(dir)
            if (info == null || info.type != FileType.DIRECTORY || info.uid != Sys.euid() || info.mode and 0b000_111_111 != 0) {
                throw LimenException(ErrorCode.INTERNAL, "$dir must be a directory of this user with mode 0700")
            }
            return dir
        }
    }
}
