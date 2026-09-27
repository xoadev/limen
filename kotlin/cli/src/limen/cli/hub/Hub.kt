package limen.cli.hub

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.IO
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.NodeResponse
import limen.core.WireJson
import limen.core.config.HubConfig
import limen.core.join.Arrival
import limen.core.join.HubFile
import limen.core.join.Invitation
import limen.core.join.JoinUrl
import limen.core.join.Keys
import limen.core.join.PendingInvite
import limen.core.join.Welcome
import limen.core.toml.TomlException
import kotlin.concurrent.AtomicReference
import kotlin.time.Clock
import kotlin.time.Duration
import kotlin.time.Duration.Companion.hours

/**
 * The hub is a directory (spec §7.2): its key, `limen.toml` with the nodes, `token` for HTTP clients and the pending
 * invitations. Everything that reads or changes it goes through here.
 */
class Hub(
    home: String,
) {
    val home: String = home.trimEnd('/')
    val configPath = "${this.home}/limen.toml"
    val keyPath = "${this.home}/id_ed25519"
    val tokenPath = "${this.home}/token"
    private val invites = "${this.home}/invites"
    private val arrivals = Mutex()

    val publicKey: String
        get() = Fs.readFollowing("$keyPath.pub")?.trim() ?: throw LimenException(ErrorCode.UNAVAILABLE, "no hub key; run `limen init`")

    fun config(): HubConfig {
        val text =
            Fs.readFollowing(configPath)
                ?: throw LimenException(ErrorCode.UNAVAILABLE, "no $configPath; run `limen init`")
        return try {
            HubConfig.parse(text).let { c ->
                Sys.env("LIMEN_PUBLIC_URL")?.takeIf { it.isNotBlank() }?.let { c.copy(publicUrl = it.trimEnd('/')) }
                    ?: c
            }
        } catch (e: TomlException) {
            throw LimenException(ErrorCode.BAD_REQUEST, "$configPath: ${e.message}")
        }
    }

    /**
     * Creates what is missing and leaves what exists: the key pair, `limen.toml` and, for `serve`, the token of the
     * HTTP clients. Returns what it created.
     */
    fun init(serve: Boolean): List<String> {
        val created = mutableListOf<String>()
        Fs.mkdirs(home, 0b111_000_000)
        if (!Fs.exists(keyPath)) {
            val keygen = Proc.which("ssh-keygen") ?: throw LimenException(ErrorCode.UNAVAILABLE, "ssh-keygen is not installed")
            val r = Proc.run(listOf(keygen, "-q", "-t", "ed25519", "-N", "", "-C", "limen-hub@${Sys.hostname()}", "-f", keyPath))
            if (r.exitCode != 0) throw LimenException(ErrorCode.INTERNAL, "ssh-keygen: ${r.err.trim()}")
            created += keyPath
        }
        if (!Fs.exists(configPath)) {
            Fs.writeAtomic(configPath, CONFIG_TEMPLATE.encodeToByteArray(), 0b110_000_000)
            created += configPath
        }
        if (serve && !Fs.exists(tokenPath) && Sys.env("LIMEN_TOKEN").isNullOrBlank()) {
            Fs.writeAtomic(tokenPath, (random(32) + "\n").encodeToByteArray(), 0b110_000_000)
            created += tokenPath
        }
        return created
    }

    /** The HTTP clients' token: `LIMEN_TOKEN`, or the one `init` wrote. */
    fun token(): String {
        val token =
            Sys.env("LIMEN_TOKEN")?.trim()?.takeIf { it.isNotEmpty() }
                ?: Fs.readFollowing(tokenPath)?.trim()?.takeIf { it.isNotEmpty() }
                ?: throw LimenException(ErrorCode.UNAVAILABLE, "no token: set LIMEN_TOKEN or run `limen init --serve`")
        if (token.length < MIN_TOKEN) {
            throw LimenException(
                ErrorCode.BAD_REQUEST,
                "the token is ${token.length} characters long; it needs $MIN_TOKEN characters or more",
            )
        }
        return token
    }

    /** A one-time invitation for [name], valid for [ttl] (spec §10.1). */
    fun invite(
        name: String,
        ttl: Duration = 1.hours,
    ): IssuedInvite {
        if (!HubConfig.NODE_NAME.matches(
                name,
            )
        ) {
            throw LimenException(ErrorCode.BAD_REQUEST, "a node name matches ${HubConfig.NODE_NAME.pattern}")
        }
        Fs.mkdirs(invites, 0b111_000_000)
        val code = random(26)
        val secret = random(26)
        val pending = PendingInvite(name, (Clock.System.now() + ttl).epochSeconds, secret)
        Fs.writeAtomic(
            "$invites/$code.json",
            WireJson.encodeToString(PendingInvite.serializer(), pending).encodeToByteArray(),
            0b110_000_000,
        )
        return IssuedInvite(code, secret)
    }

    fun pending(code: String): PendingInvite? {
        if (!JoinUrl.CODE.matches(code)) return null
        val path = "$invites/$code.json"
        val invite =
            Fs.readText(path)?.let { runCatching { LenientJson.decodeFromString(PendingInvite.serializer(), it) }.getOrNull() }
                ?: return null
        if (invite.expiresEpoch < Clock.System.now().epochSeconds) {
            Fs.remove(path)
            return null
        }
        return invite
    }

    fun invitation(code: String): Invitation? = pending(code)?.let { Invitation(it.name, publicKey) }

    /**
     * A node that used [code] arrives: its entry goes into limen.toml, the invitation is spent, and the hub tries it
     * at once, so the node's installer can say whether it worked.
     */
    suspend fun arrive(
        code: String,
        arrival: Arrival,
        from: String,
        client: NodeClient,
    ): Welcome {
        val (invite, address) = arrivals.withLock { admit(code, arrival, from) }
        val hello =
            try {
                withContext(Dispatchers.IO) { client.call(invite.name, "hello", JsonObject(emptyMap())) }
            } catch (e: LimenException) {
                NodeResponse.failure(e)
            }
        val detail =
            if (hello.ok) {
                val data = hello.data as? JsonObject

                fun field(key: String) = (data?.get(key) as? JsonPrimitive)?.contentOrNull
                "${field("os") ?: "Linux"}, limen ${field("version")}"
            } else {
                "${hello.error?.code}: ${hello.error?.message}"
            }
        return Welcome(invite.name, address, hello.ok, detail)
    }

    /** Checks the arrival, writes the node and spends the invitation: one join at a time, so neither is done twice. */
    private fun admit(
        code: String,
        arrival: Arrival,
        from: String,
    ): Pair<PendingInvite, String> {
        val invite =
            pending(code)
                ?: throw LimenException(
                    ErrorCode.NOT_FOUND,
                    "this invitation does not exist, was used, or expired; ask the hub for another",
                )
        // Refused without spending the invitation: a forged arrival must not take the real node's place, or its turn.
        if (!Http.constantTimeEquals(arrival.proof, arrival.proofWith(invite.secret))) {
            throw LimenException(ErrorCode.BAD_REQUEST, "the arrival is not signed with the join line's secret")
        }
        val address = arrival.address?.takeIf { it.isNotBlank() } ?: from.removePrefix("::ffff:")
        writeNode(invite.name, address, arrival.port, arrival.user, arrival.hostKey)
        Fs.remove("$invites/$code.json")
        return invite to address
    }

    /** Adds or replaces a node by hand (`limen trust`). */
    fun trust(
        name: String,
        address: String,
        hostKey: String,
        user: String,
        port: Int,
    ) = writeNode(name, address, port, user, hostKey)

    /**
     * Writes one node's table and nothing else. The values are checked by [HubFile.upsertNode]; then, whatever it
     * wrote, the result must parse and differ from what was there only in that node, or nothing is written.
     */
    private fun writeNode(
        name: String,
        address: String,
        port: Int,
        user: String,
        hostKey: String,
    ) {
        val text = Fs.readFollowing(configPath).orEmpty()
        val before = HubConfig.parse(text)
        val updated = HubFile.upsertNode(text, name, address, port, user, hostKey)
        val after =
            try {
                HubConfig.parse(updated)
            } catch (e: TomlException) {
                throw LimenException(ErrorCode.BAD_REQUEST, "the node does not fit the hub's configuration: ${e.message}")
            }

        fun without(c: HubConfig) = c.copy(nodes = c.nodes.filter { it.name != name })
        if (without(after) != without(before) || after.node(name) == null) {
            throw LimenException(ErrorCode.BAD_REQUEST, "adding $name would change more than $name in $configPath; refused")
        }
        Fs.writeFollowing(configPath, updated.encodeToByteArray(), 0b110_000_000)
    }

    fun remove(name: String): Boolean {
        val text = Fs.readFollowing(configPath) ?: return false
        val updated = HubFile.removeNode(text, name)
        if (updated == text) return false
        Fs.writeFollowing(configPath, updated.encodeToByteArray(), 0b110_000_000)
        return true
    }

    companion object {
        /** Spec §9. The token `init` writes has 32. */
        const val MIN_TOKEN = 16

        fun home(option: String?): String =
            option ?: Sys.env("LIMEN_HOME")?.takeIf { it.isNotBlank() } ?: ((Sys.env("HOME") ?: "/root") + "/.limen")

        fun at(option: String?) = Hub(home(option))

        /** [length] characters of base32 from the kernel's random source: 5 bits each. */
        fun random(length: Int): String {
            val alphabet = "abcdefghijklmnopqrstuvwxyz234567"
            val bytes = Fs.read("/dev/urandom", length) ?: throw LimenException(ErrorCode.INTERNAL, "cannot read /dev/urandom")
            return bytes.joinToString("") { alphabet[(it.toInt() and 0xff) % 32].toString() }
        }

        val CONFIG_TEMPLATE =
            """
            # limen hub (docs/spec.md §7.2). Nodes are added by `limen invite` and `limen trust`, or by hand.

            [ssh]
            identity = "id_ed25519"

            # [http]
            # Where nodes reach this hub to join: an address on your network or VPN, not a name.
            # public_url = "http://100.64.0.2:7341"

            """.trimIndent()
    }
}

/** An invitation as `limen invite` prints it: the code goes on the wire, the secret stays in the line's fragment. */
data class IssuedInvite(
    val code: String,
    val secret: String,
)

/**
 * A [NodeClient] that follows limen.toml: the file is read on every call and the SSH client rebuilt when it changed,
 * so a node that joins is there for the next request, with no restart.
 */
class LiveHub(
    val hub: Hub,
) : NodeClient {
    private class State(
        val text: String,
        val config: HubConfig,
        val client: SshClient,
    )

    private val state = AtomicReference<State?>(null)

    fun config(): HubConfig = current().config

    private fun current(): State {
        val text = Fs.readFollowing(hub.configPath).orEmpty()
        state.value?.let { if (it.text == text) return it }
        val config = hub.config()
        val fresh = State(text, config, SshClient(config, hub.home))
        state.value = fresh
        return fresh
    }

    override val nodes: List<String> get() = current().config.nodes.map { it.name }

    override suspend fun call(
        node: String,
        request: String,
        args: JsonObject,
        timeout: Duration?,
    ): NodeResponse = current().client.call(node, request, args, timeout)

    val ssh: SshClient get() = current().client
}
