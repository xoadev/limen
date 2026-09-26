package limen.cli.node

import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.ProcResult
import limen.cli.os.SpawnException
import limen.core.Args
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.scripts.Catalog
import limen.core.scripts.HeaderException
import limen.core.scripts.ScriptHeaders
import limen.core.scripts.ScriptKind
import limen.core.scripts.ScriptSpec
import limen.core.scripts.Trust
import kotlin.time.Duration.Companion.seconds

/** A file in a script directory: its spec when it is usable, or what is wrong with it. */
class ScriptEntry(
    val kind: ScriptKind,
    val file: String,
    val path: String,
    val spec: ScriptSpec?,
    val problem: String?,
)

/** Finds, validates and runs the operator's scripts (spec §6). */
object Scripts {
    private const val MAX_HEADER = 64 * 1024

    fun discover(
        node: Node,
        kind: ScriptKind,
    ): List<ScriptEntry> {
        val dir = node.config.directory(kind)
        val info = Fs.stat(dir) ?: return emptyList()
        if (info.type != FileType.DIRECTORY) return listOf(ScriptEntry(kind, dir, dir, null, "$dir is not a directory"))
        return Fs.list(dir).map { file -> entry(node, kind, dir, file) }
    }

    private fun entry(
        node: Node,
        kind: ScriptKind,
        dir: String,
        file: String,
    ): ScriptEntry {
        val path = "$dir/$file"
        val name =
            ScriptHeaders.nameOf(file, kind)
                ?: return ScriptEntry(
                    kind,
                    file,
                    path,
                    null,
                    "$path: not a valid script name (${if (kind == ScriptKind.SETUP) "NN-name" else "a-z, 0-9, - and _"})",
                )
        val resolved = Fs.realPath(path) ?: return ScriptEntry(kind, file, path, null, "$path: cannot resolve")
        Trust.problem(Fs.chain(resolved), node.trustedOwner)?.let { return ScriptEntry(kind, file, path, null, "$path: $it") }
        val text = Fs.read(resolved, MAX_HEADER)?.decodeToString() ?: return ScriptEntry(kind, file, path, null, "$path: cannot read")
        return try {
            ScriptEntry(kind, file, resolved, ScriptHeaders.parse(name, kind, text), null)
        } catch (e: HeaderException) {
            ScriptEntry(kind, file, path, null, "$path: ${e.message}")
        }
    }

    fun catalog(node: Node): Catalog {
        val all = ScriptKind.entries.associateWith { discover(node, it) }

        fun specs(kind: ScriptKind) = all.getValue(kind).mapNotNull { it.spec }
        return Catalog(
            checks = specs(ScriptKind.CHECK),
            actions = specs(ScriptKind.ACTION),
            setup = specs(ScriptKind.SETUP),
            problems = all.values.flatten().mapNotNull { it.problem },
        )
    }

    /** The usable script [name] of [kind]; `not_found` or `unavailable` with the reason otherwise. */
    fun find(
        node: Node,
        kind: ScriptKind,
        name: String,
    ): Pair<ScriptEntry, ScriptSpec> {
        val entry =
            discover(node, kind).firstOrNull { ScriptHeaders.nameOf(it.file, kind) == name }
                ?: throw LimenException(ErrorCode.NOT_FOUND, "no ${kind.name.lowercase()} named '$name' in ${node.config.directory(kind)}")
        val spec = entry.spec ?: throw LimenException(ErrorCode.UNAVAILABLE, entry.problem ?: "unusable script")
        return entry to spec
    }

    /** The environment of a script: a clean one plus `LIMEN_*` (spec §6). Arguments are validated first. */
    fun environment(
        node: Node,
        spec: ScriptSpec,
        args: JsonObject,
    ): List<String> {
        val values = Args.validate(spec.params, args)
        return Proc.SYSTEM_ENV + "HOME=/root" + "LIMEN_KIND=${spec.kind.name.lowercase()}" + "LIMEN_SCRIPT=${spec.name}" +
            "LIMEN_NODE=${limen.cli.os.Sys.hostname()}" +
            values.map { (k, v) -> "${ScriptHeaders.envName(k)}=${text(v)}" }
    }

    private fun text(v: JsonElement): String = (v as? JsonPrimitive)?.content ?: v.toString()

    fun run(
        entry: ScriptEntry,
        spec: ScriptSpec,
        env: List<String>,
        maxOutput: Int = 64 * 1024,
        onChunk: ((Int, ByteArray) -> Unit)? = null,
    ): ProcResult =
        try {
            Proc.run(listOf(entry.path), env = env, timeout = spec.timeoutSeconds.seconds, maxOutput = maxOutput, onChunk = onChunk)
        } catch (e: SpawnException) {
            throw LimenException(ErrorCode.INTERNAL, e.message ?: "cannot run ${entry.path}")
        }
}
