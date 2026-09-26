package limen.cli.node

import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import limen.cli.os.Fs
import limen.cli.os.Sys
import limen.core.bool
import limen.core.obj
import limen.core.scripts.ScriptKind
import limen.core.scripts.ScriptSpec
import limen.core.string
import limen.core.system.Parsers

/**
 * The deploy role (spec §6): `apply` runs the setup scripts in order, `action` one action. Output goes to stdout as
 * it comes and to `runs/<time>-<name>.log`; the answer is the exit code. Used by the gate and by `limen apply` /
 * `limen action` on the node, so both paths behave the same.
 */
object Deploy {
    fun run(
        node: Node,
        request: String,
        args: Map<String, JsonElement>,
    ): Boolean =
        when (request) {
            "apply" -> apply(node, args.string("from"), args.bool("dry_run") ?: false)
            "action" -> action(node, args.string("name")!!, args.obj("args"))
            else -> error("not a deploy request: $request")
        }

    fun apply(
        node: Node,
        from: String?,
        dryRun: Boolean,
    ): Boolean {
        val entries = Scripts.discover(node, ScriptKind.SETUP)
        // Every script is validated before the first runs: a broken one halfway would leave the machine half done.
        val problems = entries.mapNotNull { it.problem }
        if (problems.isNotEmpty()) {
            say("limen: apply stopped before running anything:\n" + problems.joinToString("\n") { "  $it" } + "\n")
            return false
        }
        val selected =
            entries.filter { e -> from == null || e.file.substringBefore('-').padStart(4, '0') >= from.padStart(4, '0') }
        if (selected.isEmpty()) {
            say("limen: no setup scripts${if (from != null) " from $from" else ""} in ${node.config.setup}\n")
            return true
        }
        if (dryRun) {
            selected.forEach { say("would run ${it.spec!!.name}: ${it.spec.description}\n") }
            return true
        }
        for (entry in selected) {
            val spec = entry.spec!!
            if (!runOne(node, entry, spec, JsonObject(emptyMap()))) {
                say("limen: apply stopped at ${spec.name}; resume with --from ${spec.name.substringBefore('-')}\n")
                return false
            }
        }
        say("limen: apply finished, ${selected.size} script(s)\n")
        return true
    }

    fun action(
        node: Node,
        name: String,
        args: JsonObject,
    ): Boolean {
        val (entry, spec) = Scripts.find(node, ScriptKind.ACTION, name)
        return runOne(node, entry, spec, args)
    }

    private fun runOne(
        node: Node,
        entry: ScriptEntry,
        spec: ScriptSpec,
        args: JsonObject,
    ): Boolean {
        val env = Scripts.environment(node, spec, args)
        val stamp = Parsers.iso(node.now().epochSeconds).replace(":", "")
        val log = "${node.config.runs}/$stamp-${spec.name}.log"
        val logOk =
            runCatching {
                Fs.mkdirs(node.config.runs, 0b111_000_000)
                Fs.appendLine(log, "# ${spec.kind.name.lowercase()} ${spec.name} ${Parsers.iso(node.now().epochSeconds)}")
            }.isSuccess
        say("==> ${spec.name}: ${spec.description}\n")
        val r =
            Scripts.run(entry, spec, env, onChunk = { _, bytes ->
                Sys.outBytes(bytes)
                if (logOk) runCatching { Fs.append(log, bytes) }
            })
        val ok = r.exitCode == 0 && !r.timedOut
        val result =
            when {
                r.timedOut -> "timed out after ${spec.timeoutSeconds}s"
                r.signal != null -> "killed by signal ${r.signal}"
                else -> "exit ${r.exitCode}"
            }
        if (logOk) runCatching { Fs.appendLine(log, "# $result") }
        say(if (ok) "<== ${spec.name}: ok\n" else "<== ${spec.name}: FAILED ($result)\n")
        return ok
    }

    private fun say(text: String) = Sys.out(text)
}
