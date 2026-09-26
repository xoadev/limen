package limen.core

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray

@Serializable
enum class ParamType {
    @SerialName("int")
    INT,

    @SerialName("bool")
    BOOL,

    @SerialName("enum")
    ENUM,

    @SerialName("string")
    STRING,

    /** Only for the nested arguments of `check` and `action`, which the script's own header validates. */
    @SerialName("object")
    OBJECT,
}

/**
 * One argument of a request or a script (spec §5, §6). The same value validates on the node and becomes the JSON
 * Schema of the MCP tool on the hub, so the two can't disagree.
 */
@Serializable
data class Param(
    val name: String,
    val type: ParamType,
    val description: String = "",
    val required: Boolean = false,
    val default: JsonPrimitive? = null,
    val min: Long? = null,
    val max: Long? = null,
    val values: List<String>? = null,
    val pattern: String? = null,
) {
    private val regex: Regex? by lazy { pattern?.let(::Regex) }

    fun jsonSchema(): JsonObject =
        buildJsonObject {
            when (type) {
                ParamType.INT -> {
                    put("type", "integer")
                    min?.let { put("minimum", it) }
                    max?.let { put("maximum", it) }
                }

                ParamType.BOOL -> {
                    put("type", "boolean")
                }

                ParamType.ENUM -> {
                    put("type", "string")
                    putJsonArray("enum") { values.orEmpty().forEach { add(JsonPrimitive(it)) } }
                }

                ParamType.STRING -> {
                    put("type", "string")
                    pattern?.let { put("pattern", it) }
                }

                ParamType.OBJECT -> {
                    put("type", "object")
                }
            }
            if (description.isNotEmpty()) put("description", description)
            default?.let { put("default", it) }
        }

    /** [value] if it fits this parameter; otherwise a `bad_request` that says why. */
    fun check(value: JsonElement): JsonElement {
        val prim = value as? JsonPrimitive
        when (type) {
            ParamType.INT -> {
                val n = prim?.takeUnless { it.isString }?.longOrNull ?: badRequest("$name must be an integer")
                if (min != null && n < min) badRequest("$name must be at least $min")
                if (max != null && n > max) badRequest("$name must be at most $max")
            }

            ParamType.BOOL -> {
                prim?.takeUnless { it.isString }?.booleanOrNull ?: badRequest("$name must be true or false")
            }

            ParamType.ENUM -> {
                val s = prim?.takeIf { it.isString }?.content ?: badRequest("$name must be a string")
                if (s !in values.orEmpty()) badRequest("$name must be one of ${values.orEmpty().joinToString(", ")}")
            }

            ParamType.STRING -> {
                val s = prim?.takeIf { it.isString }?.content ?: badRequest("$name must be a string")
                val r = regex
                if (r != null && !r.matches(s)) badRequest("$name does not match $pattern")
            }

            ParamType.OBJECT -> {
                if (value !is JsonObject) badRequest("$name must be an object")
            }
        }
        return value
    }

    companion object {
        val NAME = Regex("^[a-z][a-z0-9_]{0,31}$")

        /** What a script's string argument accepts when its header says nothing (spec §6). */
        const val DEFAULT_STRING_PATTERN = "^[A-Za-z0-9._-]{1,64}$"
    }
}

object Args {
    /**
     * [args] checked against [params]: unknown names and missing required ones are rejected, defaults filled in.
     * The result only has names from [params].
     */
    fun validate(
        params: List<Param>,
        args: JsonObject,
    ): Map<String, JsonElement> {
        val known = params.associateBy { it.name }
        args.keys.firstOrNull { it !in known }?.let { badRequest("unknown argument '$it'") }
        val out = linkedMapOf<String, JsonElement>()
        for (p in params) {
            val given = args[p.name]?.takeUnless { it is JsonPrimitive && it.isNull() }
            when {
                given != null -> out[p.name] = p.check(given)
                p.default != null -> out[p.name] = p.default
                p.required -> badRequest("missing argument '${p.name}'")
            }
        }
        return out
    }

    fun inputSchema(
        params: List<Param>,
        extra: List<Pair<Param, Boolean>> = emptyList(),
    ): JsonObject =
        buildJsonObject {
            put("type", "object")
            val all = extra.map { it.first } + params
            put("properties", JsonObject(all.associate { it.name to it.jsonSchema() }))
            val required = extra.filter { it.second }.map { it.first.name } + params.filter { it.required }.map { it.name }
            if (required.isNotEmpty()) put("required", JsonArray(required.map(::JsonPrimitive)))
            put("additionalProperties", false)
        }
}

private fun JsonPrimitive.isNull() = this is kotlinx.serialization.json.JsonNull

fun Map<String, JsonElement>.string(name: String): String? = (this[name] as? JsonPrimitive)?.takeIf { it.isString }?.content

fun Map<String, JsonElement>.long(name: String): Long? = (this[name] as? JsonPrimitive)?.longOrNull

fun Map<String, JsonElement>.int(name: String): Int? = long(name)?.toInt()

fun Map<String, JsonElement>.bool(name: String): Boolean? = (this[name] as? JsonPrimitive)?.booleanOrNull

fun Map<String, JsonElement>.obj(name: String): JsonObject = this[name] as? JsonObject ?: JsonObject(emptyMap())
