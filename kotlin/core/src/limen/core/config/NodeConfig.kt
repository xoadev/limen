package limen.core.config

import limen.core.files.Glob
import limen.core.scripts.ScriptKind
import limen.core.toml.Toml
import limen.core.toml.TomlException
import limen.core.toml.TomlReader

/**
 * `[repo]`: the Git repository a node takes its scripts, stacks and expected state from (spec §6.1). `path` is the
 * node's folder inside it.
 */
data class RepoConfig(
    val url: String,
    val branch: String = "main",
    val path: String = "",
    val dir: String = "/opt/limen/repo",
    val tokenFile: String = "/etc/limen/repo-token",
) {
    /** The node's folder in the checkout. */
    val base: String get() = if (path.isEmpty()) dir else "$dir/$path"

    /** The URL without credentials, for answers and logs. */
    val displayUrl: String get() = url.replace(Regex("^(\\w+://)[^/@]*@"), "$1")

    /** Owner and name when the repository is on GitHub, for the token link of `install`. */
    val github: Pair<String, String>? get() =
        Regex("^(?:https://github\\.com/|git@github\\.com:|ssh://git@github\\.com/)([^/]+)/([^/]+?)(?:\\.git)?/?$")
            .matchEntire(url)
            ?.let { it.groupValues[1] to it.groupValues[2] }

    /** Last `sync`: next to the checkout, outside it, so `git clean` never removes it. */
    val syncRecord: String get() = dir.trimEnd('/').substringBeforeLast('/') + "/last-sync.json"
}

/** `/etc/limen/limen.toml` (spec §7.1). Every key has a default; a missing file means nothing is readable. */
data class NodeConfig(
    val allow: List<String> = emptyList(),
    val deny: List<String> = emptyList(),
    val maxFileBytes: Int = 256 * 1024,
    val maxLines: Int = 2000,
    val scanLines: Int = 100_000,
    val maxResponseBytes: Int = 1024 * 1024,
    val redact: List<String> = emptyList(),
    val explicitChecks: String? = null,
    val explicitActions: String? = null,
    val explicitSetup: String? = null,
    val audit: String = "/var/log/limen/audit.jsonl",
    val auditMaxBytes: Long = 5L * 1024 * 1024,
    val runs: String = "/var/log/limen/runs",
    val repo: RepoConfig? = null,
) {
    /** Script directories: what `[scripts]` says, else the repository's folder, else `/etc/limen/<kind>.d`. */
    val checks: String get() = explicitChecks ?: repo?.let { "${it.base}/checks" } ?: "/etc/limen/checks.d"
    val actions: String get() = explicitActions ?: repo?.let { "${it.base}/actions" } ?: "/etc/limen/actions.d"
    val setup: String get() = explicitSetup ?: repo?.let { "${it.base}/setup" } ?: "/etc/limen/setup.d"

    /** `stacks/<name>/compose.yaml` in the node's folder of the repository. */
    val stacks: String? get() = repo?.let { "${it.base}/stacks" }

    /** What must be running: `node.toml` in the node's folder of the repository. */
    val expectations: String? get() = repo?.let { "${it.base}/node.toml" }

    fun directory(kind: ScriptKind): String =
        when (kind) {
            ScriptKind.CHECK -> checks
            ScriptKind.ACTION -> actions
            ScriptKind.SETUP -> setup
        }

    companion object {
        const val PATH = "/etc/limen/limen.toml"
        private val REPO_URL = Regex("^(https://|ssh://|git@|file://)[^\\s]+$")
        private val REPO_PATH = Regex("^([A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*)?$")
        private val BRANCH = Regex("^[A-Za-z0-9._/-]{1,100}$")

        fun parse(text: String): NodeConfig {
            val root = TomlReader(Toml.parse(text))
            val d = NodeConfig()
            val files = root.table("files")
            val logs = root.table("logs")
            val limits = root.table("limits")
            val redact = root.table("redact")
            val scripts = root.table("scripts")
            val audit = root.table("audit")
            val repo = root.table("repo")
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
                    explicitChecks = scripts?.string("checks")?.also { absolute(scripts, "checks", it) },
                    explicitActions = scripts?.string("actions")?.also { absolute(scripts, "actions", it) },
                    explicitSetup = scripts?.string("setup")?.also { absolute(scripts, "setup", it) },
                    audit = audit?.string("path")?.also { absolute(audit, "path", it) } ?: d.audit,
                    auditMaxBytes =
                        audit?.long("max_bytes")?.also { if (it < 1024) audit.fail("max_bytes", "at least 1024") } ?: d.auditMaxBytes,
                    runs = audit?.string("runs")?.also { absolute(audit, "runs", it) } ?: d.runs,
                    repo = repo?.let(::repo),
                )
            listOfNotNull(files, logs, limits, redact, scripts, audit, repo).forEach { it.rejectUnknown() }
            root.rejectUnknown()
            return config
        }

        private fun repo(t: TomlReader): RepoConfig {
            val d = RepoConfig("")
            val url = t.string("url") ?: t.fail("url", "missing")
            if (!REPO_URL.matches(url)) t.fail("url", "expected an https://, ssh://, git@ or file:// URL")
            val branch = t.string("branch") ?: d.branch
            if (!BRANCH.matches(branch) || branch.startsWith("-")) t.fail("branch", "not a branch name")
            val path = (t.string("path") ?: d.path).trim('/')
            if (!REPO_PATH.matches(path) ||
                path.split('/').any { it == ".." || it == "." }
            ) {
                t.fail("path", "a relative folder of the repository")
            }
            val dir = (t.string("dir") ?: d.dir).trimEnd('/')
            absolute(t, "dir", dir)
            if (dir.count { it == '/' } < 2) t.fail("dir", "too close to /; it is replaced on every sync")
            val tokenFile = t.string("token_file") ?: d.tokenFile
            absolute(t, "token_file", tokenFile)
            return RepoConfig(url, branch, path, dir, tokenFile)
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
