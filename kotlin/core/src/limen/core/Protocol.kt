package limen.core

import kotlinx.serialization.EncodeDefault
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject

/** The node protocol versions this build speaks (spec §4). */
val PROTOCOL_VERSIONS = listOf(1)

/** One request: a JSON object on the gate's stdin (spec §4). */
@Serializable
data class NodeRequest(
    val v: Int,
    val request: String,
    val args: JsonObject = JsonObject(emptyMap()),
)

@Serializable
data class NodeError(
    val code: String,
    val message: String,
    val versions: List<Int>? = null,
)

/** The gate's answer on stdout. `truncated` says an output limit cut the data (spec §1, principle 5). */
@OptIn(ExperimentalSerializationApi::class)
@Serializable
data class NodeResponse(
    val ok: Boolean,
    val data: JsonElement? = null,
    @EncodeDefault(EncodeDefault.Mode.NEVER) val truncated: Boolean = false,
    val error: NodeError? = null,
) {
    companion object {
        fun success(
            data: JsonElement,
            truncated: Boolean = false,
        ) = NodeResponse(ok = true, data = data, truncated = truncated)

        fun failure(e: LimenException) = NodeResponse(ok = false, error = NodeError(e.code.wire, e.message ?: e.code.wire, e.versions))
    }
}

enum class ErrorCode(
    val wire: String,
) {
    BAD_REQUEST("bad_request"),
    DENIED("denied"),
    NOT_FOUND("not_found"),
    UNAVAILABLE("unavailable"),
    TIMEOUT("timeout"),
    UNSUPPORTED_VERSION("unsupported_version"),
    INTERNAL("internal"),

    // Only the hub produces these two: they are what `ssh` itself says when it cannot reach the gate.
    UNREACHABLE("unreachable"),
    HOST_KEY_MISMATCH("host_key_mismatch"),
}

class LimenException(
    val code: ErrorCode,
    message: String,
    val versions: List<Int>? = null,
) : Exception(message)

fun badRequest(message: String): Nothing = throw LimenException(ErrorCode.BAD_REQUEST, message)

/** The JSON of the wire: strict on input, so a field nobody reads is an error and not a silent no-op. */
@OptIn(ExperimentalSerializationApi::class)
val WireJson =
    Json {
        ignoreUnknownKeys = false
        explicitNulls = false
        encodeDefaults = true
    }

/** For reading what other programs print (`docker inspect`, `journalctl -o json`): those add fields over time. */
val LenientJson = Json { ignoreUnknownKeys = true }

val PrettyJson =
    Json {
        prettyPrint = true
        prettyPrintIndent = "  "
    }
