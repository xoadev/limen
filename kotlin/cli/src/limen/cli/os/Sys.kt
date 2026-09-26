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

    fun chdirRoot() {
        platform.posix.chdir("/")
    }
}
