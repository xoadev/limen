package limen.core.config

import limen.core.files.Glob
import limen.core.scripts.ScriptKind
import limen.core.toml.Toml
import limen.core.toml.TomlException
import limen.core.toml.TomlReader

/** `/etc/limen/limen.toml` (spec §7.1). Every key has a default; a missing file means nothing is readable. */
data class NodeConfig(
    val allow: List<String> = emptyList(),
    val deny: List<String> = emptyList(),
    val maxFileBytes: Int = 256 * 1024,
    val maxLines: Int = 2000,
    val scanLines: Int = 100_000,
    val maxResponseBytes: Int = 1024 * 1024,
    val redact: List<String> = emptyList(),
    val checks: String = "/etc/limen/checks.d",
    val actions: String = "/etc/limen/actions.d",
    val setup: String = "/etc/limen/setup.d",
    val audit: String = "/var/log/limen/audit.jsonl",
    val runs: String = "/var/log/limen/runs",
) {
    fun directory(kind: ScriptKind): String =
        when (kind) {
            ScriptKind.CHECK -> checks
            ScriptKind.ACTION -> actions
            ScriptKind.SETUP -> setup
        }

    companion object {
        const val PATH = "/etc/limen/limen.toml"

        fun parse(text: String): NodeConfig {
            val root = TomlReader(Toml.parse(text))
            val d = NodeConfig()
            val files = root.table("files")
            val logs = root.table("logs")
            val limits = root.table("limits")
            val redact = root.table("redact")
            val scripts = root.table("scripts")
            val audit = root.table("audit")
            val config =
                NodeConfig(
                    allow = files?.strings("allow")?.also { patterns(files, "allow", it) } ?: d.allow,
                    deny = files?.strings("deny")?.also { patterns(files, "deny", it) } ?: d.deny,
                    maxFileBytes = files?.int("max_bytes")?.also { positive(files, "max_bytes", it) } ?: d.maxFileBytes,
                    maxLines = logs?.int("max_lines")?.also { positive(logs, "max_lines", it) } ?: d.maxLines,
                    scanLines = logs?.int("scan_lines")?.also { positive(logs, "scan_lines", it) } ?: d.scanLines,
                    maxResponseBytes = limits?.int("max_response")?.also { positive(limits, "max_response", it) } ?: d.maxResponseBytes,
                    redact =
                        redact?.strings("patterns")?.also { list ->
                            list.forEach { p -> runCatching { Regex(p) }.onFailure { redact.fail("patterns", "bad regex '$p'") } }
                        } ?: d.redact,
                    checks = scripts?.string("checks")?.also { absolute(scripts, "checks", it) } ?: d.checks,
                    actions = scripts?.string("actions")?.also { absolute(scripts, "actions", it) } ?: d.actions,
                    setup = scripts?.string("setup")?.also { absolute(scripts, "setup", it) } ?: d.setup,
                    audit = audit?.string("path")?.also { absolute(audit, "path", it) } ?: d.audit,
                    runs = audit?.string("runs")?.also { absolute(audit, "runs", it) } ?: d.runs,
                )
            listOfNotNull(files, logs, limits, redact, scripts, audit).forEach { it.rejectUnknown() }
            root.rejectUnknown()
            return config
        }

        private fun patterns(
            table: TomlReader,
            key: String,
            list: List<String>,
        ) = list.forEach { p ->
            try {
                Glob(p)
            } catch (e: IllegalArgumentException) {
                table.fail(key, e.message ?: "bad pattern")
            }
        }

        private fun positive(
            table: TomlReader,
            key: String,
            value: Int,
        ) {
            if (value <= 0) table.fail(key, "must be positive")
        }

        private fun absolute(
            table: TomlReader,
            key: String,
            value: String,
        ) {
            if (!value.startsWith("/")) table.fail(key, "must be an absolute path")
        }

        /** The error, prefixed with the file, for a gate that must answer JSON even when its configuration is broken. */
        fun describe(e: TomlException) = "$PATH: ${e.message}"
    }
}
