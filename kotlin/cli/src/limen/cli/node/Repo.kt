package limen.cli.node

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put
import limen.cli.os.FileType
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.ProcResult
import limen.cli.os.SpawnException
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.WireJson
import limen.core.config.RepoConfig
import limen.core.system.Parsers
import kotlin.io.encoding.Base64
import kotlin.time.Duration
import kotlin.time.Duration.Companion.minutes
import kotlin.time.Duration.Companion.seconds

/** What `git` says about a commit. */
data class Commit(
    val hash: String,
    val date: String,
    val subject: String,
)

/**
 * The node's copy of its repository (spec §6.1). `sync` makes it what the remote branch has —local edits and
 * untracked files discarded, ignored ones such as a stack's `.env` kept— because the repository is the source of
 * truth, not the machine.
 *
 * The token never goes in a URL or an argument, where `ps` and logs would show it: git gets it as an HTTP header
 * through `GIT_CONFIG_*` in its environment.
 */
object Repo {
    fun sync(
        node: Node,
        repo: RepoConfig,
    ): Pair<String?, String> {
        val before = deployed(repo)?.hash
        val token = token(repo)
        val result =
            runCatching {
                if (Fs.stat("${repo.dir}/.git")?.type != FileType.DIRECTORY) {
                    Fs.mkdirs(repo.dir.substringBeforeLast('/'), 0b111_101_101)
                    if (Fs.exists(repo.dir)) Proc.run(listOf(which("rm"), "-rf", "--", repo.dir))
                    git(
                        repo,
                        token,
                        null,
                        "clone",
                        "--quiet",
                        "--depth",
                        "1",
                        "--single-branch",
                        "--branch",
                        repo.branch,
                        "--",
                        repo.url,
                        repo.dir,
                    )
                } else {
                    // The URL of limen.toml, not the one of the first clone: changing [repo].url moves the node.
                    git(repo, token, repo.dir, "remote", "set-url", "origin", repo.url)
                    git(repo, token, repo.dir, "fetch", "--quiet", "--depth", "1", "origin", "--", "refs/heads/${repo.branch}")
                    git(repo, token, repo.dir, "reset", "--quiet", "--hard", "FETCH_HEAD")
                    // Untracked files go; ignored ones stay: a stack's `.env`, kept out of the repository on purpose.
                    git(repo, token, repo.dir, "clean", "--quiet", "-ffd")
                }
                deployed(repo)?.hash ?: throw LimenException(ErrorCode.INTERNAL, "git: no commit after sync")
            }
        record(node, repo, before, result.getOrNull(), result.exceptionOrNull()?.message)
        return before to result.getOrThrow()
    }

    fun deployed(repo: RepoConfig): Commit? {
        if (Fs.stat("${repo.dir}/.git")?.type != FileType.DIRECTORY) return null
        val r = runCatching { git(repo, null, repo.dir, "log", "-1", "--format=%H%n%cI%n%s") }.getOrNull() ?: return null
        val lines = r.out.lines()
        return if (lines.size >= 3) Commit(lines[0], lines[1], lines[2]) else null
    }

    /** The commit the remote branch points at, asked with `ls-remote`: nothing on the node changes. */
    fun remote(repo: RepoConfig): String {
        val r = git(repo, token(repo), null, "ls-remote", "--", repo.url, "refs/heads/${repo.branch}", timeout = 30.seconds)
        return r.out
            .lineSequence()
            .firstOrNull { it.isNotBlank() }
            ?.substringBefore('\t')
            ?: throw LimenException(ErrorCode.NOT_FOUND, "the remote has no branch ${repo.branch}")
    }

    sealed interface Access {
        data object Readable : Access

        /** The server wants credentials, or these ones are not enough: a token can fix it. */
        data class NeedsToken(
            val message: String,
        ) : Access

        /** Anything else —network, TLS, a wrong URL—: a token would not help, and asking for one would mislead. */
        data class Failed(
            val message: String,
        ) : Access
    }

    /** Whether [token] (or none) can read the repository: what `install` checks before saving it. */
    fun access(
        repo: RepoConfig,
        token: String?,
    ): Access =
        try {
            git(repo, token, null, "ls-remote", "--", repo.url, "refs/heads/${repo.branch}", timeout = 30.seconds)
            Access.Readable
        } catch (e: LimenException) {
            val message = e.message ?: "git ls-remote failed"
            if (AUTH.containsMatchIn(message)) Access.NeedsToken(message) else Access.Failed(message)
        }

    // What git says when credentials are missing or refused, prompts being off. GitHub answers "not found" for a
    // private repository it won't show.
    private val AUTH =
        Regex("could not read Username|Authentication failed|terminal prompts disabled|Repository not found|returned error: 40[134]")

    fun lastSync(repo: RepoConfig): JsonObject? =
        Fs.readText(repo.syncRecord)?.let { runCatching { LenientJson.parseToJsonElement(it).jsonObject }.getOrNull() }

    fun token(repo: RepoConfig): String? = Fs.readText(repo.tokenFile)?.trim()?.ifEmpty { null }

    private fun record(
        node: Node,
        repo: RepoConfig,
        from: String?,
        to: String?,
        error: String?,
    ) {
        val entry =
            buildJsonObject {
                put("time", Parsers.iso(node.now().epochSeconds))
                put("result", if (error == null) "ok" else "failed")
                put("from", from)
                put("to", to)
                error?.let { put("error", it) }
            }
        runCatching {
            Fs.writeAtomic(
                repo.syncRecord,
                WireJson.encodeToString(JsonObject.serializer(), entry).encodeToByteArray(),
                0b110_100_100,
            )
        }
    }

    private fun git(
        repo: RepoConfig,
        token: String?,
        dir: String?,
        vararg args: String,
        timeout: Duration = 10.minutes,
    ): ProcResult {
        val argv = listOf(which("git")) + (if (dir != null) listOf("-C", dir) else emptyList()) + args
        val r =
            try {
                Proc.run(argv, env = env(repo, token), timeout = timeout)
            } catch (e: SpawnException) {
                throw LimenException(ErrorCode.INTERNAL, e.message ?: "cannot run git")
            }
        if (r.timedOut) throw LimenException(ErrorCode.TIMEOUT, "git ${args.first()} did not finish in $timeout")
        if (r.exitCode != 0) {
            val message =
                r.err
                    .trim()
                    .lines()
                    .lastOrNull { it.isNotBlank() } ?: "exit ${r.exitCode}"
            throw LimenException(ErrorCode.UNAVAILABLE, "git ${args.first()}: ${message.replace(token ?: "\u0000", "[redacted]")}")
        }
        return r
    }

    private fun env(
        repo: RepoConfig,
        token: String?,
    ): List<String> {
        // No prompt ever, no system or user configuration: what git does is what limen asks.
        val base = Proc.ROOT_ENV + listOf("GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null")
        if (token == null || !repo.url.startsWith("https://")) return base
        val basic = Base64.encode("x-access-token:$token".encodeToByteArray())
        val host = repo.url.removePrefix("https://").substringBefore('/')
        return base +
            listOf(
                "GIT_CONFIG_COUNT=1",
                "GIT_CONFIG_KEY_0=http.https://$host/.extraheader",
                "GIT_CONFIG_VALUE_0=Authorization: Basic $basic",
            )
    }

    private fun which(name: String) = Proc.which(name) ?: throw LimenException(ErrorCode.UNAVAILABLE, "$name is not installed on this node")
}
