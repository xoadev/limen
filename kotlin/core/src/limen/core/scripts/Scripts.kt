package limen.core.scripts

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonPrimitive
import limen.core.Durations
import limen.core.Param
import limen.core.ParamType
import limen.core.Requests
import limen.core.toml.Toml
import limen.core.toml.TomlBool
import limen.core.toml.TomlException
import limen.core.toml.TomlInt
import limen.core.toml.TomlReader
import limen.core.toml.TomlString
import limen.core.toml.TomlValue
import kotlin.time.Duration
import kotlin.time.Duration.Companion.hours
import kotlin.time.Duration.Companion.seconds

@Serializable
enum class ScriptKind(
    val directory: String,
    val defaultTimeout: Duration,
) {
    @SerialName("check")
    CHECK("checks", 60.seconds),

    @SerialName("action")
    ACTION("actions", 1.hours),

    @SerialName("setup")
    SETUP("setup", 1.hours),
}

/** What a script says about itself in its `#:` header (spec §6). */
@Serializable
data class ScriptSpec(
    val name: String,
    val kind: ScriptKind,
    val description: String,
    @SerialName("timeout_seconds") val timeoutSeconds: Long,
    val params: List<Param> = emptyList(),
)

/** The scripts of a node, and what is wrong with the ones that could not be read. Part of `hello`. */
@Serializable
data class Catalog(
    val checks: List<ScriptSpec> = emptyList(),
    val actions: List<ScriptSpec> = emptyList(),
    val setup: List<ScriptSpec> = emptyList(),
    val problems: List<String> = emptyList(),
)

class HeaderException(
    message: String,
) : Exception(message)

object ScriptHeaders {
    private val nameRegex = Regex(Requests.SCRIPT_NAME)
    private val setupName = Regex("^[0-9]{1,4}-.+$")

    /** The script name of a file: its name without the extension, or null when that is not a valid name. */
    fun nameOf(
        fileName: String,
        kind: ScriptKind,
    ): String? {
        val base = if (fileName.startsWith(".")) return null else fileName.substringBeforeLast('.')
        if (!nameRegex.matches(base)) return null
        if (kind == ScriptKind.SETUP && !setupName.matches(base)) return null
        return base
    }

    /** The `#:` lines of the leading comment block, without the marker: a TOML document. The header is never run. */
    fun extract(text: String): String? {
        val lines = text.lineSequence().iterator()
        val out = StringBuilder()
        var first = true
        var found = false
        while (lines.hasNext()) {
            val line = lines.next()
            if (first && line.startsWith("#!")) {
                first = false
                continue
            }
            first = false
            if (!line.startsWith("#")) break
            if (line.startsWith("#:")) {
                found = true
                out.append(line.removePrefix("#:").removePrefix(" ")).append('\n')
            }
        }
        return if (found) out.toString() else null
    }

    fun parse(
        name: String,
        kind: ScriptKind,
        text: String,
    ): ScriptSpec {
        val header = extract(text) ?: throw HeaderException("$name: no `#:` header")
        try {
            val root = TomlReader(Toml.parse(header))
            val description = root.string("description") ?: throw HeaderException("$name: the header has no description")
            val timeout =
                root.string("timeout")?.let {
                    Durations.parse(it) ?: throw HeaderException("$name: timeout '$it' is not a duration like 30s or 5m")
                } ?: kind.defaultTimeout
            val params =
                root
                    .table("args")
                    ?.tables()
                    ?.map { (argName, arg) -> param(name, argName, arg) }
                    .orEmpty()
            root.rejectUnknown()
            return ScriptSpec(name, kind, description, timeout.inWholeSeconds.coerceAtLeast(1), params)
        } catch (e: TomlException) {
            throw HeaderException("$name: header: ${e.message}")
        }
    }

    private fun param(
        script: String,
        name: String,
        arg: TomlReader,
    ): Param {
        if (!Param.NAME.matches(name)) throw HeaderException("$script: argument name '$name' must match ${Param.NAME.pattern}")
        val type =
            when (val t = arg.string("type")) {
                "int" -> ParamType.INT
                "bool" -> ParamType.BOOL
                "enum" -> ParamType.ENUM
                "string" -> ParamType.STRING
                null -> throw HeaderException("$script: argument '$name' has no type")
                else -> throw HeaderException("$script: argument '$name' has type '$t'; it is int, bool, enum or string")
            }
        val description = arg.string("description").orEmpty()
        val range = if (type == ParamType.INT) arg.longs("range") else null
        if (range != null && (range.size != 2 || range[0] > range[1])) {
            throw HeaderException("$script: argument '$name': range is [min, max]")
        }
        val values = if (type == ParamType.ENUM) arg.strings("values") else null
        if (type == ParamType.ENUM && values.isNullOrEmpty()) throw HeaderException("$script: argument '$name' is an enum without values")
        val pattern =
            if (type == ParamType.STRING) {
                (arg.string("pattern") ?: Param.DEFAULT_STRING_PATTERN).also {
                    runCatching { Regex(it) }.onFailure { throw HeaderException("$script: argument '$name': bad pattern") }
                }
            } else {
                null
            }
        val default = arg.raw("default")?.let { primitive(script, name, it) }
        val required = arg.bool("required") ?: (default == null)
        arg.rejectUnknown()
        val param = Param(name, type, description, required, default, range?.get(0), range?.get(1), values, pattern)
        if (default != null) {
            runCatching { param.check(default) }.onFailure {
                throw HeaderException("$script: argument '$name': the default does not fit (${it.message})")
            }
        }
        return param
    }

    private fun primitive(
        script: String,
        name: String,
        value: TomlValue,
    ): JsonPrimitive =
        when (value) {
            is TomlString -> JsonPrimitive(value.value)
            is TomlInt -> JsonPrimitive(value.value)
            is TomlBool -> JsonPrimitive(value.value)
            else -> throw HeaderException("$script: argument '$name': default must be a string, integer or boolean")
        }

    /** `threshold` → `LIMEN_ARG_THRESHOLD`: how an argument reaches the script (spec §6). */
    fun envName(param: String) = "LIMEN_ARG_" + param.uppercase()
}
