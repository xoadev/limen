package limen.cli.node

import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.LimenException
import limen.core.bool
import limen.core.obj
import limen.core.scripts.ScriptKind
import limen.core.scripts.ScriptSpec
import limen.core.string
import limen.core.system.Parsers
import kotlin.time.Duration.Companion.minutes

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
            "sync" -> sync(node)
            "apply" -> apply(node, args.string("from"), args.bool("dry_run") ?: false, args.bool("sync") ?: true)
            "action" -> action(node, args.string("name")!!, args.obj("args"))
            else -> error("not a deploy request: $request")
        }

    fun sync(node: Node): Boolean {
        val repo = node.config.repo ?: return say("limen: no [repo] in limen.toml; nothing to sync\n").let { false }
        say("==> sync ${repo.displayUrl} ${repo.branch}${if (repo.path.isEmpty()) "" else " (${repo.path})"}\n")
        return try {
            val (from, to) = Repo.sync(node, repo)
            say(if (from == to) "<== sync: already at ${to.take(12)}\n" else "<== sync: ${from?.take(12) ?: "nothing"} -> ${to.take(12)}\n")
            true
        } catch (e: LimenException) {
            say("<== sync: FAILED (${e.message})\n")
            false
        }
    }

    /**
     * From the repository to a running node (spec §6.1): sync, the setup scripts in order, the compose stacks
     * node.toml declares, and then `state`, which must say that everything expected runs.
     */
    fun apply(
        node: Node,
        from: String?,
        dryRun: Boolean,
        syncFirst: Boolean = true,
    ): Boolean {
        if (syncFirst && !dryRun && node.config.repo != null && !sync(node)) return false
        val entries = Scripts.discover(node, ScriptKind.SETUP)
        // Every script is validated before the first runs: a broken one halfway would leave the machine half done.
        val problems = entries.mapNotNull { it.problem }
        if (problems.isNotEmpty()) {
            say("limen: apply stopped before running anything:\n" + problems.joinToString("\n") { "  $it" } + "\n")
            return false
        }
        val selected =
            entries.filter { e -> from == null || e.file.substringBefore('-').padStart(4, '0') >= from.padStart(4, '0') }
        val stacks =
            try {
                State.expectations(node).compose
            } catch (e: LimenException) {
                say("limen: apply stopped: ${e.message}\n")
                return false
            }
        if (dryRun) {
            selected.forEach { say("would run ${it.spec!!.name}: ${it.spec.description}\n") }
            stacks.forEach { say("would bring up stack $it\n") }
            return true
        }
        for (entry in selected) {
            val spec = entry.spec!!
            if (!runOne(node, entry, spec, JsonObject(emptyMap()))) {
                say("limen: apply stopped at ${spec.name}; resume with --from ${spec.name.substringBefore('-')}\n")
                return false
            }
        }
        for (stack in stacks) {
            if (!up(node, stack)) {
                say("limen: apply stopped at stack $stack\n")
                return false
            }
        }
        val down = State.services(node).filter { !it.running }
        down.forEach { say("limen: expected ${it.kind} ${it.name} is not running\n") }
        say(
            if (down.isEmpty()) {
                "limen: apply finished: ${selected.size} script(s), ${stacks.size} stack(s)\n"
            } else {
                "limen: apply finished with ${down.size} expected service(s) not running\n"
            },
        )
        return down.isEmpty()
    }

    /** `docker compose up` of one stack, streamed like a script. */
    private fun up(
        node: Node,
        stack: String,
    ): Boolean {
        val file =
            try {
                State.composeFile(node, stack)
            } catch (e: LimenException) {
                say("<== stack $stack: FAILED (${e.message})\n")
                return false
            }
        val docker = Proc.which("docker") ?: return say("<== stack $stack: FAILED (docker is not installed)\n").let { false }
        say("==> stack $stack\n")
        val r =
            Proc.run(
                listOf(docker, "compose", "-p", stack, "-f", file, "up", "-d", "--remove-orphans"),
                env = Proc.SYSTEM_ENV + "HOME=/root",
                timeout = 30.minutes,
                onChunk = { _, bytes -> Sys.outBytes(bytes) },
            )
        val ok = r.exitCode == 0 && !r.timedOut
        say(if (ok) "<== stack $stack: ok\n" else "<== stack $stack: FAILED (exit ${r.exitCode})\n")
        return ok
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
