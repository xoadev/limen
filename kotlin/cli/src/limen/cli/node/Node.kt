package limen.cli.node

import kotlinx.serialization.json.JsonElement
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.cli.os.ProcResult
import limen.cli.os.SpawnException
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.Redactor
import limen.core.config.NodeConfig
import limen.core.files.PathPolicy
import limen.core.toml.TomlException
import kotlin.time.Clock
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds
import kotlin.time.Instant

/** Everything a request on this node is answered with: its configuration and what derives from it. */
class Node(
    val config: NodeConfig,
) {
    val redactor = Redactor(config.redact)
    val policy = PathPolicy(config.allow, config.deny)

    /** Scripts must belong to the user limen runs as: root in production (spec §6). */
    val trustedOwner: Int = Sys.euid()

    fun now(): Instant = Clock.System.now()

    /**
     * Runs a system program by name. A program that is not there is `unavailable` (no Docker on this node), not
     * an internal error.
     */
    fun exec(
        vararg argv: String,
        timeout: Duration = 30.seconds,
        maxOutput: Int = 16 * 1024 * 1024,
    ): ProcResult {
        val path = Proc.which(argv[0]) ?: throw LimenException(ErrorCode.UNAVAILABLE, "${argv[0]} is not installed on this node")
        val result =
            try {
                Proc.run(listOf(path) + argv.drop(1), env = Proc.ROOT_ENV, timeout = timeout, maxOutput = maxOutput)
            } catch (e: SpawnException) {
                throw LimenException(ErrorCode.INTERNAL, e.message ?: "cannot run ${argv[0]}")
            }
        if (result.timedOut) throw LimenException(ErrorCode.TIMEOUT, "${argv[0]} did not finish in $timeout")
        return result
    }

    /** [exec] that must succeed; its stderr becomes the error. */
    fun execOk(
        vararg argv: String,
        timeout: Duration = 30.seconds,
    ): String {
        val r = exec(*argv, timeout = timeout)
        if (r.exitCode !=
            0
        ) {
            throw LimenException(
                ErrorCode.INTERNAL,
                "${argv[0]}: ${r.err
                    .trim()
                    .lines()
                    .lastOrNull() ?: "exit ${r.exitCode}"}",
            )
        }
        return r.out
    }

    companion object {
        fun load(path: String): Node {
            val text = Fs.readFollowing(path) ?: return Node(NodeConfig())
            return try {
                Node(NodeConfig.parse(text))
            } catch (e: TomlException) {
                throw LimenException(ErrorCode.INTERNAL, "$path: ${e.message}")
            }
        }
    }
}

/** What a request handler returns: the data, and whether a limit cut it. */
class Answer(
    val data: JsonElement,
    val truncated: Boolean = false,
)
