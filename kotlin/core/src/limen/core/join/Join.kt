package limen.core.join

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import limen.core.badRequest
import limen.core.config.HubConfig
import kotlin.io.encoding.Base64

/**
 * Joining a node to a hub (spec §10.1): the hub hands out a line with a one-time code and the fingerprint of its
 * key; the node downloads the key, checks it against the fingerprint, installs, and tells the hub its host key.
 */
object Keys {
    private val PUBLIC_KEY = Regex("^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(256|384|521)) ([A-Za-z0-9+/]+=*)( .*)?$")

    /** `SHA256:<base64>` of the key blob, as `ssh-keygen -lf` prints it: what a person can check with standard tools. */
    fun fingerprint(publicKey: String): String {
        val blob = PUBLIC_KEY.matchEntire(publicKey.trim())?.groupValues?.get(3) ?: badRequest("not an SSH public key")
        val bytes = runCatching { Base64.decode(blob) }.getOrElse { badRequest("not an SSH public key") }
        return "SHA256:" + Base64.encode(Sha256.digest(bytes)).trimEnd('=')
    }

    /** `<type> <base64>`, without the comment: how a host key is written in the hub's limen.toml. */
    fun withoutComment(publicKey: String): String =
        publicKey
            .trim()
            .split(' ')
            .take(2)
            .joinToString(" ")
}

/** `http://100.64.0.2:7341/join/<code>#SHA256:…`, the line `limen invite` prints. */
data class JoinUrl(
    val base: String,
    val code: String,
    val fingerprint: String,
    /** Never sent: it signs the node's arrival, so whoever sees the code on the wire can't arrive in its place. */
    val secret: String,
) {
    val host: String get() = base.removePrefix("http://").substringBeforeLast(':')
    val port: Int get() = base.removePrefix("http://").substringAfterLast(':').toInt()

    override fun toString() = "$base/join/$code#$fingerprint.$secret"

    companion object {
        val CODE = Regex("^[a-z2-7]{26}$")

        // An address and not a name: the static binary resolves no names (tools/ld-static).
        private val URL =
            Regex(
                "^(http://(?:\\d{1,3}(?:\\.\\d{1,3}){3}|\\[[0-9a-fA-F:]+\\]):\\d{1,5})/join/([a-z2-7]{26})#(SHA256:[A-Za-z0-9+/]{43})\\.([a-z2-7]{26})$",
            )

        fun parse(text: String): JoinUrl {
            val m =
                URL.matchEntire(text.trim())
                    ?: badRequest(
                        "not a join line from `limen invite`: expected http://<address>:<port>/join/<code>#SHA256:<fingerprint>.<secret>",
                    )
            return JoinUrl(m.groupValues[1], m.groupValues[2], m.groupValues[3], m.groupValues[4])
        }
    }
}

/** What `GET /join/<code>` answers. */
@Serializable
data class Invitation(
    val name: String,
    @SerialName("hub_key") val hubKey: String,
)

/** What a node sends with `POST /join/<code>` once installed. */
@Serializable
data class Arrival(
    @SerialName("host_key") val hostKey: String,
    val user: String,
    val port: Int = 22,
    /** Where the hub reaches it; without it, the address the request came from. */
    val address: String? = null,
    /** HMAC-SHA256 of the fields above with the join line's secret, hex. */
    val proof: String = "",
) {
    fun proofWith(secret: String): String =
        Sha256.toHex(
            Sha256.hmac(secret.encodeToByteArray(), listOf(hostKey, user, port, address.orEmpty()).joinToString("\n").encodeToByteArray()),
        )

    fun signed(secret: String) = copy(proof = proofWith(secret))
}

/** What the hub answers to an [Arrival]. */
@Serializable
data class Welcome(
    val name: String,
    val address: String,
    val reachable: Boolean,
    val detail: String,
)

/** An invitation as the hub keeps it until it is used or expires. */
@Serializable
data class PendingInvite(
    val name: String,
    @SerialName("expires_epoch") val expiresEpoch: Long,
    val secret: String,
)

/**
 * The hub's limen.toml, edited as text so that everything else in it —comments, order, other settings— stays as
 * the operator wrote it: a node's `[nodes.<name>]` table is replaced or appended, nothing more.
 *
 * Every value is checked against the pattern the configuration itself enforces before it is written: they come from
 * the node that joins, and a quote and a newline in an address would otherwise let it write TOML of its own —another
 * node, or `[http]` settings—.
 */
object HubFile {
    fun upsertNode(
        text: String,
        name: String,
        host: String,
        port: Int,
        user: String,
        hostKey: String,
    ): String {
        val key = Keys.withoutComment(hostKey)
        if (!HubConfig.NODE_NAME.matches(name)) badRequest("'$name' is not a node name")
        if (!HubConfig.HOST.matches(host)) badRequest("the address is not an address or host name")
        if (port !in 1..65535) badRequest("the port is out of range")
        if (!HubConfig.USER.matches(user)) badRequest("the user is not a user name")
        if (!HubConfig.HOST_KEY.matches(key)) badRequest("the host key is not '<type> <base64>'")
        val section =
            buildString {
                append("[nodes.$name]\n")
                append("host = \"$host\"\n")
                if (port != 22) append("port = $port\n")
                if (user != HubConfig.READ_USER) append("user = \"$user\"\n")
                append("host_key = \"$key\"\n")
            }
        val kept = removeNode(text, name).trimEnd()
        return (if (kept.isEmpty()) "" else "$kept\n\n") + section
    }

    fun removeNode(
        text: String,
        name: String,
    ): String {
        val out = mutableListOf<String>()
        var skipping = false
        val header = Regex("^\\s*\\[\\s*nodes\\.\"?${Regex.escape(name)}\"?\\s*]\\s*(#.*)?$")
        for (line in text.lines()) {
            if (line.trimStart().startsWith("[")) skipping = header.matches(line)
            if (!skipping) out += line
        }
        return out.joinToString("\n").replace(Regex("\n{3,}"), "\n\n")
    }
}
