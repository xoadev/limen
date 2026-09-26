package limen.cli.node

import limen.cli.os.Sys
import limen.core.scripts.ScriptKind

/** `limen lint` (spec §10): every script directory checked without running anything. Exit 1 when something is wrong. */
object Lint {
    fun run(node: Node): Int {
        var problems = 0
        for (kind in ScriptKind.entries) {
            val dir = node.config.directory(kind)
            val entries = Scripts.discover(node, kind)
            Sys.out("$dir: ${entries.size} file(s)\n")
            for (e in entries) {
                if (e.problem != null) {
                    problems++
                    Sys.out("  FAIL ${e.problem}\n")
                } else {
                    Sys.out(
                        "  ok   ${e.spec!!.name}: ${e.spec.description} (${e.spec.params.size} argument(s), timeout ${e.spec.timeoutSeconds}s)\n",
                    )
                }
            }
        }
        Sys.out(if (problems == 0) "lint: OK\n" else "lint: $problems problem(s)\n")
        return if (problems == 0) 0 else 1
    }
}
