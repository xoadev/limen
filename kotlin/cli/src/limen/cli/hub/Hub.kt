package limen.cli.hub

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.IO
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

    val publicKey: String
        get() = Fs.readText("$keyPath.pub")?.trim() ?: throw LimenException(ErrorCode.UNAVAILABLE, "no hub key; run `limen init`")

    fun config(): HubConfig {
        val text =
            Fs.realPath(configPath)?.let { Fs.readText(it) }
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
    fun token(): String =
        Sys.env("LIMEN_TOKEN")?.trim()?.takeIf { it.isNotEmpty() }
            ?: Fs.readText(tokenPath)?.trim()?.takeIf { it.isNotEmpty() }
            ?: throw LimenException(ErrorCode.UNAVAILABLE, "no token: set LIMEN_TOKEN or run `limen init --serve`")

    /** A one-time invitation for [name], valid for [ttl] (spec §10.1). */
    fun invite(
        name: String,
        ttl: Duration = 1.hours,
    ): String {
        if (!HubConfig.NODE_NAME.matches(
                name,
            )
        ) {
            throw LimenException(ErrorCode.BAD_REQUEST, "a node name matches ${HubConfig.NODE_NAME.pattern}")
        }
        Fs.mkdirs(invites, 0b111_000_000)
        val code = random(26)
        val pending = PendingInvite(name, (Clock.System.now() + ttl).epochSeconds)
        Fs.writeAtomic(
            "$invites/$code.json",
            WireJson.encodeToString(PendingInvite.serializer(), pending).encodeToByteArray(),
            0b110_000_000,
        )
        return code
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
        val invite =
            pending(code)
                ?: throw LimenException(
                    ErrorCode.NOT_FOUND,
                    "this invitation does not exist, was used, or expired; ask the hub for another",
                )
        val address = arrival.address?.takeIf { it.isNotBlank() } ?: from.removePrefix("::ffff:")
        val entry = HubFile.upsertNode(Fs.readText(configPath).orEmpty(), invite.name, address, arrival.port, arrival.user, arrival.hostKey)
        // Parsed before it is written: a host key or address that would break the hub's file is refused instead.
        try {
            HubConfig.parse(entry)
        } catch (e: TomlException) {
            throw LimenException(ErrorCode.BAD_REQUEST, "the node's answer does not fit the hub's configuration: ${e.message}")
        }
        Fs.writeAtomic(configPath, entry.encodeToByteArray(), 0b110_000_000)
        Fs.remove("$invites/$code.json")
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

    /** Adds or replaces a node by hand (`limen trust`). */
    fun trust(
        name: String,
        address: String,
        hostKey: String,
        user: String,
        port: Int,
    ) {
        Keys.fingerprint(hostKey)
        val text = HubFile.upsertNode(Fs.readText(configPath).orEmpty(), name, address, port, user, hostKey)
        try {
            HubConfig.parse(text)
        } catch (e: TomlException) {
            throw LimenException(ErrorCode.BAD_REQUEST, e.message ?: "bad node")
        }
        Fs.writeAtomic(configPath, text.encodeToByteArray(), 0b110_000_000)
    }

    fun remove(name: String): Boolean {
        val text = Fs.readText(configPath) ?: return false
        val updated = HubFile.removeNode(text, name)
        if (updated == text) return false
        Fs.writeAtomic(configPath, updated.encodeToByteArray(), 0b110_000_000)
        return true
    }

    companion object {
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
        val text = Fs.readText(hub.configPath).orEmpty()
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
    ): NodeResponse = current().client.call(node, request, args)

    val ssh: SshClient get() = current().client
}
