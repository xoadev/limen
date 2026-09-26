package limen.cli.os

import kotlinx.cinterop.ByteVar
import kotlinx.cinterop.ExperimentalForeignApi
import kotlinx.cinterop.addressOf
import kotlinx.cinterop.alloc
import kotlinx.cinterop.allocArray
import kotlinx.cinterop.convert
import kotlinx.cinterop.memScoped
import kotlinx.cinterop.pointed
import kotlinx.cinterop.ptr
import kotlinx.cinterop.toKString
import kotlinx.cinterop.usePinned
import limen.core.scripts.FileStat
import platform.posix.EEXIST
import platform.posix.O_APPEND
import platform.posix.O_CLOEXEC
import platform.posix.O_CREAT
import platform.posix.O_NOFOLLOW
import platform.posix.O_RDONLY
import platform.posix.O_TRUNC
import platform.posix.O_WRONLY
import platform.posix.SEEK_SET
import platform.posix.S_IFDIR
import platform.posix.S_IFLNK
import platform.posix.S_IFMT
import platform.posix.S_IFREG
import platform.posix.X_OK
import platform.posix.access
import platform.posix.chmod
import platform.posix.chown
import platform.posix.close
import platform.posix.closedir
import platform.posix.errno
import platform.posix.fstat
import platform.posix.fsync
import platform.posix.getgrgid
import platform.posix.getpwnam
import platform.posix.getpwuid
import platform.posix.lseek
import platform.posix.lstat
import platform.posix.mkdir
import platform.posix.open
import platform.posix.opendir
import platform.posix.read
import platform.posix.readdir
import platform.posix.realpath
import platform.posix.rename
import platform.posix.stat
import platform.posix.strerror
import platform.posix.unlink
import platform.posix.write

class FsException(
    message: String,
) : Exception(message)

enum class FileType(
    val wire: String,
) {
    FILE("file"),
    DIRECTORY("dir"),
    LINK("link"),
    OTHER("other"),
}

data class FileInfo(
    val path: String,
    val type: FileType,
    val size: Long,
    val mode: Int,
    val uid: Int,
    val gid: Int,
    val modifiedEpochSeconds: Long,
) {
    fun toStat() = FileStat(path, uid, mode and 0xFFF, type == FileType.DIRECTORY, type == FileType.FILE)
}

/** A slice of a file's lines, and whether there was more of it. */
class LineSlice(
    val lines: List<String>,
    val eof: Boolean,
    val binary: Boolean,
)

/** Files through POSIX: what the gate reads and what `install` writes. */
@OptIn(ExperimentalForeignApi::class)
object Fs {
    private const val PATH_MAX = 4096
    private const val CHUNK = 64 * 1024

    fun realPath(path: String): String? =
        memScoped {
            val buf = allocArray<ByteVar>(PATH_MAX)
            realpath(path, buf)?.toKString()
        }

    /** [path] itself, not what a link points to. */
    fun lstat(path: String): FileInfo? =
        memScoped {
            val st = alloc<stat>()
            if (lstat(path, st.ptr) != 0) return null
            info(path, st)
        }

    fun stat(path: String): FileInfo? =
        memScoped {
            val st = alloc<stat>()
            if (stat(path, st.ptr) != 0) return null
            info(path, st)
        }

    private fun info(
        path: String,
        st: stat,
    ): FileInfo {
        val mode = st.st_mode.toInt()
        val type =
            when (mode and S_IFMT) {
                S_IFREG -> FileType.FILE
                S_IFDIR -> FileType.DIRECTORY
                S_IFLNK -> FileType.LINK
                else -> FileType.OTHER
            }
        return FileInfo(path, type, st.st_size, mode, st.st_uid.toInt(), st.st_gid.toInt(), st.st_mtim.tv_sec)
    }

    fun exists(path: String) = lstat(path) != null

    fun isExecutable(path: String) = stat(path)?.type == FileType.FILE && access(path, X_OK) == 0

    fun list(dir: String): List<String> {
        val d = opendir(dir) ?: throw FsException("cannot open $dir: ${lastError()}")
        try {
            val names = mutableListOf<String>()
            while (true) {
                val entry = readdir(d) ?: break
                val name = entry.pointed.d_name.toKString()
                if (name != "." && name != "..") names += name
            }
            return names.sorted()
        } finally {
            closedir(d)
        }
    }

    /**
     * At most [max] bytes of [path]. Opened with `O_NOFOLLOW`: the caller resolved the path and checked it, and a
     * link swapped in since then must not be followed.
     */
    fun read(
        path: String,
        max: Int = Int.MAX_VALUE,
    ): ByteArray? {
        val fd = open(path, O_RDONLY or O_CLOEXEC or O_NOFOLLOW)
        if (fd < 0) return null
        try {
            val out = mutableListOf<ByteArray>()
            var total = 0
            val buffer = ByteArray(CHUNK)
            while (total < max) {
                val n = buffer.usePinned { read(fd, it.addressOf(0), minOf(CHUNK, max - total).convert()).toInt() }
                if (n <= 0) break
                out += buffer.copyOf(n)
                total += n
            }
            return join(out, total)
        } finally {
            close(fd)
        }
    }

    fun readText(path: String): String? = read(path)?.decodeToString()

    /**
     * Lines [from]..[from]+[count]-1 of [path] (1-based), reading in chunks so a big file costs what is read, and
     * never more than [maxBytes] of content.
     */
    fun readLines(
        path: String,
        from: Int,
        count: Int,
        maxBytes: Int,
    ): LineSlice {
        val fd = open(path, O_RDONLY or O_CLOEXEC or O_NOFOLLOW)
        if (fd < 0) throw FsException("cannot open $path: ${lastError()}")
        try {
            val lines = mutableListOf<String>()
            var lineNo = 1
            var bytes = 0
            val partial = mutableListOf<ByteArray>()
            var partialSize = 0
            val buffer = ByteArray(CHUNK)
            var first = true
            while (true) {
                val n = buffer.usePinned { read(fd, it.addressOf(0), CHUNK.convert()).toInt() }
                if (n <= 0) break
                if (first && buffer.copyOf(minOf(n, 8192)).contains(0)) return LineSlice(emptyList(), eof = true, binary = true)
                first = false
                var start = 0
                for (i in 0 until n) {
                    if (buffer[i] != '\n'.code.toByte()) continue
                    if (lineNo >= from) {
                        val line = join(partial + buffer.copyOfRange(start, i), partialSize + i - start).decodeToString()
                        bytes += line.length + 1
                        if (bytes > maxBytes || lines.size >= count) return LineSlice(lines, eof = false, binary = false)
                        lines += line
                    }
                    partial.clear()
                    partialSize = 0
                    lineNo++
                    start = i + 1
                }
                if (lineNo >= from && start < n) {
                    partial += buffer.copyOfRange(start, n)
                    partialSize += n - start
                    if (partialSize > maxBytes) return LineSlice(lines, eof = false, binary = false)
                }
            }
            if (partialSize > 0 && lineNo >= from) {
                if (lines.size >= count) return LineSlice(lines, eof = false, binary = false)
                lines += join(partial, partialSize).decodeToString()
            }
            return LineSlice(lines, eof = true, binary = false)
        } finally {
            close(fd)
        }
    }

    /** The last [count] lines of [path], reading backwards from the end at most [maxBytes]. */
    fun tail(
        path: String,
        count: Int,
        maxBytes: Long,
    ): LineSlice {
        val info = stat(path) ?: throw FsException("cannot read $path")
        val fd = open(path, O_RDONLY or O_CLOEXEC or O_NOFOLLOW)
        if (fd < 0) throw FsException("cannot open $path: ${lastError()}")
        try {
            val start = maxOf(0L, info.size - maxBytes)
            lseek(fd, start, SEEK_SET)
            val bytes = mutableListOf<ByteArray>()
            var total = 0
            val buffer = ByteArray(CHUNK)
            while (true) {
                val n = buffer.usePinned { read(fd, it.addressOf(0), CHUNK.convert()).toInt() }
                if (n <= 0) break
                bytes += buffer.copyOf(n)
                total += n
            }
            val all = join(bytes, total)
            if (all.copyOf(minOf(all.size, 8192)).contains(0)) return LineSlice(emptyList(), eof = true, binary = true)
            var lines = all.decodeToString().split('\n')
            if (start > 0) lines = lines.drop(1)
            if (lines.lastOrNull()?.isEmpty() == true) lines = lines.dropLast(1)
            return LineSlice(lines.takeLast(count), eof = start == 0L && lines.size <= count, binary = false)
        } finally {
            close(fd)
        }
    }

    fun appendLine(
        path: String,
        line: String,
        mode: Int = 0b110_000_000,
    ) = append(path, (line + "\n").encodeToByteArray(), mode)

    fun append(
        path: String,
        bytes: ByteArray,
        mode: Int = 0b110_000_000,
    ) {
        val fd = open(path, O_WRONLY or O_APPEND or O_CREAT or O_CLOEXEC, mode.convert<UInt>())
        if (fd < 0) throw FsException("cannot open $path: ${lastError()}")
        try {
            writeAll(fd, bytes, path)
        } finally {
            close(fd)
        }
    }

    /** Replaces [path] with [bytes] through a temporary file and `rename`, so a reader never sees half of it. */
    fun writeAtomic(
        path: String,
        bytes: ByteArray,
        mode: Int,
    ) {
        val tmp = "$path.limen-tmp"
        val fd = open(tmp, O_WRONLY or O_CREAT or O_TRUNC or O_CLOEXEC, mode.convert<UInt>())
        if (fd < 0) throw FsException("cannot write $tmp: ${lastError()}")
        try {
            writeAll(fd, bytes, tmp)
            fsync(fd)
        } finally {
            close(fd)
        }
        chmod(tmp, mode.convert())
        if (rename(tmp, path) != 0) {
            unlink(tmp)
            throw FsException("cannot replace $path: ${lastError()}")
        }
    }

    fun mkdirs(
        path: String,
        mode: Int,
    ) {
        var current = ""
        for (part in path.split('/').filter { it.isNotEmpty() }) {
            current += "/$part"
            if (mkdir(current, mode.convert()) != 0 && errno != EEXIST) throw FsException("cannot create $current: ${lastError()}")
        }
    }

    fun chmod(
        path: String,
        mode: Int,
    ) {
        if (platform.posix.chmod(path, mode.convert()) != 0) throw FsException("cannot chmod $path: ${lastError()}")
    }

    fun chown(
        path: String,
        uid: Int,
        gid: Int,
    ) {
        if (platform.posix.chown(path, uid.convert(), gid.convert()) != 0) throw FsException("cannot chown $path: ${lastError()}")
    }

    fun remove(path: String): Boolean = unlink(path) == 0

    fun userName(uid: Int): String? = getpwuid(uid.convert())?.pointed?.pw_name?.toKString()

    fun groupName(gid: Int): String? = getgrgid(gid.convert())?.pointed?.gr_name?.toKString()

    fun shell(name: String): String? = getpwnam(name)?.pointed?.pw_shell?.toKString()

    /** uid, gid and home of [name], or null when there is no such user. */
    fun user(name: String): Triple<Int, Int, String>? =
        getpwnam(name)?.pointed?.let { Triple(it.pw_uid.toInt(), it.pw_gid.toInt(), it.pw_dir?.toKString() ?: "/") }

    /** [path] and every directory above it, as [FileStat]s: what [limen.core.scripts.Trust] checks. */
    fun chain(path: String): List<FileStat> {
        val out = mutableListOf<FileStat>()
        var current = path
        while (true) {
            out += (stat(current) ?: return out).toStat()
            if (current == "/") return out
            current = current.substringBeforeLast('/').ifEmpty { "/" }
        }
    }

    private fun writeAll(
        fd: Int,
        bytes: ByteArray,
        path: String,
    ) {
        var offset = 0
        while (offset < bytes.size) {
            val n = bytes.usePinned { write(fd, it.addressOf(offset), (bytes.size - offset).convert()).toInt() }
            if (n <= 0) throw FsException("cannot write $path: ${lastError()}")
            offset += n
        }
    }

    private fun join(
        parts: List<ByteArray>,
        total: Int,
    ): ByteArray {
        val all = ByteArray(total)
        var at = 0
        for (p in parts) {
            p.copyInto(all, at)
            at += p.size
        }
        return all
    }

    private fun lastError(): String = strerror(errno)?.toKString() ?: "errno $errno"
}
