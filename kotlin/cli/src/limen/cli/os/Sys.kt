package limen.cli.os

import kotlinx.cinterop.ByteVar
import kotlinx.cinterop.ExperimentalForeignApi
import kotlinx.cinterop.addressOf
import kotlinx.cinterop.alloc
import kotlinx.cinterop.allocArray
import kotlinx.cinterop.memScoped
import kotlinx.cinterop.ptr
import kotlinx.cinterop.toKString
import kotlinx.cinterop.usePinned
import platform.posix.fflush
import platform.posix.fputs
import platform.posix.fwrite
import platform.posix.getenv
import platform.posix.geteuid
import platform.posix.gethostname
import platform.posix.stderr
import platform.posix.stdout
import platform.posix.uname
import platform.posix.utsname

/** Process-level facts and the standard streams. */
@OptIn(ExperimentalForeignApi::class)
object Sys {
    fun env(name: String): String? = getenv(name)?.toKString()

    fun euid(): Int = geteuid().toInt()

    fun hostname(): String =
        memScoped {
            val buf = allocArray<ByteVar>(256)
            if (gethostname(buf, 255u) == 0) buf.toKString() else "unknown"
        }

    /** Kernel release and machine, as `uname -r` and `uname -m` say. */
    fun uname(): Pair<String, String> =
        memScoped {
            val u = alloc<utsname>()
            uname(u.ptr)
            u.release.toKString() to u.machine.toKString()
        }

    fun out(text: String) {
        fputs(text, stdout)
        fflush(stdout)
    }

    fun err(text: String) {
        fputs(text, stderr)
        fflush(stderr)
    }

    fun outBytes(bytes: ByteArray) {
        if (bytes.isEmpty()) return
        bytes.usePinned { fwrite(it.addressOf(0), 1u, bytes.size.toULong(), stdout) }
        fflush(stdout)
    }

    /** All of stdin, up to [max] bytes; null when there is more than that. */
    fun readStdin(max: Int): ByteArray? {
        val out = mutableListOf<ByteArray>()
        var total = 0
        val buffer = ByteArray(64 * 1024)
        while (true) {
            val n = buffer.usePinned { platform.posix.read(0, it.addressOf(0), buffer.size.toULong()).toInt() }
            if (n <= 0) break
            total += n
            if (total > max) return null
            out += buffer.copyOf(n)
            // One request is one line (spec §4): stop at the newline instead of waiting for the client to close.
            if (buffer[n - 1] == '\n'.code.toByte()) break
        }
        val all = ByteArray(total)
        var at = 0
        for (p in out) {
            p.copyInto(all, at)
            at += p.size
        }
        return all
    }

    /** Clock ticks per second, the unit of `/proc/<pid>/stat` times. */
    fun ticksPerSecond(): Long = platform.posix.sysconf(platform.posix._SC_CLK_TCK).takeIf { it > 0 } ?: 100

    fun isTerminal(fd: Int): Boolean = platform.posix.isatty(fd) == 1

    /** One line from stdin without echoing it, for a token typed at a terminal. */
    fun readSecret(): String? =
        memScoped {
            val saved = alloc<platform.posix.termios>()
            val tty = platform.posix.tcgetattr(0, saved.ptr) == 0
            if (tty) {
                val quiet = alloc<platform.posix.termios>()
                platform.posix.tcgetattr(0, quiet.ptr)
                quiet.c_lflag = quiet.c_lflag and
                    platform.posix.ECHO
                        .toUInt()
                        .inv()
                platform.posix.tcsetattr(0, platform.posix.TCSANOW, quiet.ptr)
            }
            try {
                readlnOrNull()
            } finally {
                if (tty) {
                    platform.posix.tcsetattr(0, platform.posix.TCSANOW, saved.ptr)
                    err("\n")
                }
            }
        }

    fun chdirRoot() {
        platform.posix.chdir("/")
    }
}
