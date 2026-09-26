package limen.core.config

import limen.core.Durations
import limen.core.toml.Toml
import limen.core.toml.TomlReader
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds

data class NodeEntry(
    val name: String,
    val host: String,
    val port: Int = 22,
    val user: String = HubConfig.READ_USER,
    val hostKey: String,
)

/** `$LIMEN_HOME/limen.toml` (spec §7.2): how the hub reaches its nodes and how it listens. */
data class HubConfig(
    val identity: String = "id_ed25519",
    val connectTimeout: Duration = 5.seconds,
    val requestTimeout: Duration = 60.seconds,
    val perNodeConcurrency: Int = 4,
    val listen: String = "127.0.0.1:7341",
    val origins: List<String> = emptyList(),
    val nodes: List<NodeEntry> = emptyList(),
) {
    fun node(name: String): NodeEntry? = nodes.firstOrNull { it.name == name }

    val listenHost: String get() = listen.substringBeforeLast(':')
    val listenPort: Int get() = listen.substringAfterLast(':').toInt()

    companion object {
        const val READ_USER = "limen-read"
        val NODE_NAME = Regex("^[a-z0-9][a-z0-9_-]{0,31}$")
        private val HOST_KEY = Regex("^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)|sk-ssh-ed25519@openssh\\.com) [A-Za-z0-9+/]+=*$")
        private val HOST = Regex("^[A-Za-z0-9.:_-]{1,253}$")
        private val USER = Regex("^[a-z_][a-z0-9_-]{0,31}$")
        private val LISTEN = Regex("^[^\\s]+:[0-9]{1,5}$")

        fun parse(text: String): HubConfig {
            val root = TomlReader(Toml.parse(text))
            val d = HubConfig()
            val ssh = root.table("ssh")
            val http = root.table("http")
            val nodesTable = root.table("nodes")
            val nodes =
                nodesTable
                    ?.tables()
                    ?.map { (name, t) ->
                        if (!NODE_NAME.matches(name)) nodesTable.fail(name, "a node name matches ${NODE_NAME.pattern}")
                        val host = t.string("host") ?: t.fail("host", "missing")
                        if (!HOST.matches(host)) t.fail("host", "not a host name or address")
                        val port = t.int("port") ?: 22
                        if (port !in 1..65535) t.fail("port", "out of range")
                        val user = t.string("user") ?: READ_USER
                        if (!USER.matches(user)) t.fail("user", "not a user name")
                        val key = t.string("host_key")?.trim() ?: t.fail("host_key", "missing; get it with ssh-keyscan and verify it")
                        if (!HOST_KEY.matches(key)) t.fail("host_key", "expected '<type> <base64>', as in known_hosts without the host")
                        t.rejectUnknown()
                        NodeEntry(name, host, port, user, key)
                    }.orEmpty()
            val config =
                HubConfig(
                    identity = ssh?.string("identity") ?: d.identity,
                    connectTimeout = ssh?.let { duration(it, "connect_timeout") } ?: d.connectTimeout,
                    requestTimeout = ssh?.let { duration(it, "request_timeout") } ?: d.requestTimeout,
                    perNodeConcurrency =
                        ssh?.int("per_node_concurrency")?.also {
                            if (it !in 1..64) ssh.fail("per_node_concurrency", "must be between 1 and 64")
                        } ?: d.perNodeConcurrency,
                    listen =
                        http?.string("listen")?.also {
                            if (!LISTEN.matches(it)) http.fail("listen", "expected host:port")
                        } ?: d.listen,
                    origins = http?.strings("origins") ?: d.origins,
                    nodes = nodes,
                )
            listOfNotNull(ssh, http).forEach { it.rejectUnknown() }
            root.rejectUnknown()
            return config
        }

        private fun duration(
            table: TomlReader,
            key: String,
        ): Duration? = table.string(key)?.let { Durations.parse(it) ?: table.fail(key, "expected a duration like 5s or 1m") }
    }
}
