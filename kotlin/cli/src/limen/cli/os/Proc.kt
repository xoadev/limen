package limen.cli.os

import kotlinx.cinterop.ByteVar
import kotlinx.cinterop.CPointer
import kotlinx.cinterop.CPointerVar
import kotlinx.cinterop.ExperimentalForeignApi
import kotlinx.cinterop.IntVar
import kotlinx.cinterop.addressOf
import kotlinx.cinterop.alloc
import kotlinx.cinterop.allocArray
import kotlinx.cinterop.convert
import kotlinx.cinterop.cstr
import kotlinx.cinterop.get
import kotlinx.cinterop.memScoped
import kotlinx.cinterop.ptr
import kotlinx.cinterop.set
import kotlinx.cinterop.toKString
import kotlinx.cinterop.usePinned
import kotlinx.cinterop.value
import platform.linux.POSIX_SPAWN_SETPGROUP
import platform.linux.POSIX_SPAWN_SETSIGDEF
import platform.linux.POSIX_SPAWN_SETSIGMASK
import platform.linux.posix_spawn
import platform.linux.posix_spawn_file_actions_adddup2
import platform.linux.posix_spawn_file_actions_destroy
import platform.linux.posix_spawn_file_actions_init
import platform.linux.posix_spawn_file_actions_t
import platform.linux.posix_spawnattr_destroy
import platform.linux.posix_spawnattr_init
import platform.linux.posix_spawnattr_setflags
import platform.linux.posix_spawnattr_setpgroup
import platform.linux.posix_spawnattr_setsigdefault
import platform.linux.posix_spawnattr_setsigmask
import platform.linux.posix_spawnattr_t
import platform.posix.EAGAIN
import platform.posix.EINTR
import platform.posix.FD_CLOEXEC
import platform.posix.F_GETFL
import platform.posix.F_SETFD
import platform.posix.F_SETFL
import platform.posix.O_CLOEXEC
import platform.posix.O_NONBLOCK
import platform.posix.O_RDONLY
import platform.posix.POLLERR
import platform.posix.POLLHUP
import platform.posix.POLLIN
import platform.posix.POLLOUT
import platform.posix.SIGINT
import platform.posix.SIGKILL
import platform.posix.SIGPIPE
import platform.posix.SIGQUIT
import platform.posix.SIGTERM
import platform.posix.WNOHANG
import platform.posix.close
import platform.posix.errno
import platform.posix.fcntl
import platform.posix.kill
import platform.posix.open
import platform.posix.pipe
import platform.posix.poll
import platform.posix.pollfd
import platform.posix.read
import platform.posix.sigaddset
import platform.posix.sigemptyset
import platform.posix.sigset_t
import platform.posix.strerror
import platform.posix.waitpid
import platform.posix.write
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds
import kotlin.time.TimeSource

class ProcResult(
    /** Exit status, or -1 when a signal ended the process. */
    val exitCode: Int,
    val signal: Int?,
    val stdout: ByteArray,
    val stderr: ByteArray,
    val timedOut: Boolean,
    /** Output went over the cap: the process was stopped and what came after is lost. */
    val truncated: Boolean,
) {
    val ok: Boolean get() = exitCode == 0 && !timedOut
    val out: String get() = stdout.decodeToString()
    val err: String get() = stderr.decodeToString()
}

class SpawnException(
    message: String,
) : Exception(message)

/**
 * Child processes without a shell (spec §12): `posix_spawn` with an argument array, stdout and stderr on separate
 * pipes read with a cap, a timeout that stops the whole process group, and an environment that is exactly the one
 * given. `posix_spawn` and not `fork`: after a fork only async-signal-safe calls are allowed until `exec`, and the
 * Kotlin/Native runtime is not one of them.
 */
@OptIn(ExperimentalForeignApi::class)
object Proc {
    private const val CHUNK = 64 * 1024

    private val KILL_GRACE = 2.seconds

    /** What root's programs and scripts get: [SYSTEM_ENV] and root's home. */
    val ROOT_ENV: List<String> get() = SYSTEM_ENV + "HOME=/root"

    /** What a command the gate runs sees: a fixed PATH, C locale with UTF-8, UTC, no pagers or colours. */
    val SYSTEM_ENV =
        listOf(
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "LANG=C.UTF-8",
            "LC_ALL=C.UTF-8",
            "TZ=UTC",
            "SYSTEMD_PAGER=cat",
            "SYSTEMD_COLORS=0",
            "PAGER=cat",
            "NO_COLOR=1",
        )

    /**
     * Runs [argv] to completion. [argv]`[0]` is an absolute path (see [which]). [onChunk] sees every block of output
     * as it arrives — fd 1 or 2 — which is how `apply` streams; when it is given, the output is not kept.
     */
    fun run(
        argv: List<String>,
        env: List<String> = SYSTEM_ENV,
        stdin: ByteArray? = null,
        timeout: Duration = 60.seconds,
        maxOutput: Int = 16 * 1024 * 1024,
        onChunk: ((fd: Int, bytes: ByteArray) -> Unit)? = null,
    ): ProcResult =
        memScoped {
            require(argv.isNotEmpty() && argv[0].startsWith("/")) { "argv[0] must be an absolute path: $argv" }
            if (stdin != null) ignoreSigpipe()
            val outPipe = allocArray<IntVar>(2)
            val errPipe = allocArray<IntVar>(2)
            val inPipe = allocArray<IntVar>(2)
            check(pipe(outPipe) == 0 && pipe(errPipe) == 0) { "pipe: ${lastError()}" }
            if (stdin != null) check(pipe(inPipe) == 0) { "pipe: ${lastError()}" }
            val parentFds = listOfNotNull(outPipe[0], errPipe[0], if (stdin != null) inPipe[1] else null)
            val childFds = listOfNotNull(outPipe[1], errPipe[1], if (stdin != null) inPipe[0] else null)
            (parentFds + childFds).forEach { fcntl(it, F_SETFD, FD_CLOEXEC) }

            val devNull = if (stdin == null) open("/dev/null", O_RDONLY or O_CLOEXEC) else -1
            val actions = alloc<posix_spawn_file_actions_t>()
            posix_spawn_file_actions_init(actions.ptr)
            if (stdin != null) {
                posix_spawn_file_actions_adddup2(actions.ptr, inPipe[0], 0)
            } else {
                // Opened here and dup'ed in the child, not `addopen`: glibc before 2.29 —the one tools/ld-static
                // links in— keeps addopen's path pointer until the spawn, and Kotlin/Native frees that string as soon
                // as the call returns, so the child opened garbage and exited 127.
                posix_spawn_file_actions_adddup2(actions.ptr, devNull, 0)
            }
            posix_spawn_file_actions_adddup2(actions.ptr, outPipe[1], 1)
            posix_spawn_file_actions_adddup2(actions.ptr, errPipe[1], 2)

            // Own process group, so a timeout reaches what a script started too; default signal handling and an
            // empty mask, whatever this process inherited.
            val attr = alloc<posix_spawnattr_t>()
            posix_spawnattr_init(attr.ptr)
            val defaults = alloc<sigset_t>()
            sigemptyset(defaults.ptr)
            listOf(SIGPIPE, SIGINT, SIGTERM, SIGQUIT).forEach { sigaddset(defaults.ptr, it) }
            val empty = alloc<sigset_t>()
            sigemptyset(empty.ptr)
            posix_spawnattr_setsigdefault(attr.ptr, defaults.ptr)
            posix_spawnattr_setsigmask(attr.ptr, empty.ptr)
            posix_spawnattr_setpgroup(attr.ptr, 0)
            posix_spawnattr_setflags(attr.ptr, (POSIX_SPAWN_SETPGROUP or POSIX_SPAWN_SETSIGDEF or POSIX_SPAWN_SETSIGMASK).convert())

            val cArgv = cStrings(argv)
            val cEnv = cStrings(env)
            val pid = alloc<IntVar>()
            val rc = posix_spawn(pid.ptr, argv[0], actions.ptr, attr.ptr, cArgv, cEnv)
            posix_spawn_file_actions_destroy(actions.ptr)
            posix_spawnattr_destroy(attr.ptr)
            childFds.forEach { close(it) }
            if (devNull >= 0) close(devNull)
            if (rc != 0) {
                parentFds.forEach { close(it) }
                throw SpawnException("cannot run ${argv[0]}: ${strerror(rc)?.toKString()}")
            }
            Running(pid.value, outPipe[0], errPipe[0], if (stdin != null) inPipe[1] else -1, stdin, timeout, maxOutput, onChunk).await()
        }

    /**
     * Writing to a child that already exited must be an error to handle, not the default of SIGPIPE, which kills
     * this process. Called at startup and before any run that writes to a child.
     */
    fun ignoreSigpipe() {
        platform.posix.signal(SIGPIPE, platform.posix.SIG_IGN)
    }

    /** The absolute path of [name] in [SYSTEM_ENV]'s PATH, or null. */
    fun which(name: String): String? =
        if (name.startsWith("/")) {
            name.takeIf { Fs.isExecutable(it) }
        } else {
            SYSTEM_ENV
                .first { it.startsWith("PATH=") }
                .removePrefix("PATH=")
                .split(':')
                .map { "$it/$name" }
                .firstOrNull { Fs.isExecutable(it) }
        }

    private fun kotlinx.cinterop.MemScope.cStrings(list: List<String>): CPointer<CPointerVar<ByteVar>> {
        val array = allocArray<CPointerVar<ByteVar>>(list.size + 1)
        list.forEachIndexed { i, s -> array[i] = s.cstr.getPointer(this) }
        array[list.size] = null
        return array
    }

    private class Running(
        val pid: Int,
        val outFd: Int,
        val errFd: Int,
        var inFd: Int,
        val input: ByteArray?,
        val timeout: Duration,
        val maxOutput: Int,
        val onChunk: ((Int, ByteArray) -> Unit)?,
    ) {
        private val out = Buffer()
        private val err = Buffer()
        private var inputOffset = 0
        private var truncated = false
        private var timedOut = false
        private var status: Int? = null

        fun await(): ProcResult {
            val start = TimeSource.Monotonic.markNow()
            if (inFd >= 0) fcntl(inFd, F_SETFL, fcntl(inFd, F_GETFL) or O_NONBLOCK)
            var outOpen = true
            var errOpen = true
            // After the child exits, what a grandchild keeps open must not hold the answer: drain what is
            // already there and stop.
            var exitedAt: kotlin.time.TimeMark? = null
            memScoped {
                val fds = allocArray<pollfd>(3)
                val buffer = ByteArray(CHUNK)
                while (outOpen || errOpen) {
                    if (status == null) reap(WNOHANG)
                    if (status != null && exitedAt == null) exitedAt = TimeSource.Monotonic.markNow()
                    if (exitedAt?.let { it.elapsedNow().inWholeMilliseconds > 200 } == true) break
                    if (start.elapsedNow() > timeout) {
                        timedOut = true
                        stop()
                        break
                    }
                    var n = 0
                    val slots = mutableListOf<Int>()
                    if (outOpen) {
                        fds[n].fd = outFd
                        fds[n].events = POLLIN.toShort()
                        slots += outFd
                        n++
                    }
                    if (errOpen) {
                        fds[n].fd = errFd
                        fds[n].events = POLLIN.toShort()
                        slots += errFd
                        n++
                    }
                    if (inFd >= 0) {
                        fds[n].fd = inFd
                        fds[n].events = POLLOUT.toShort()
                        slots += inFd
                        n++
                    }
                    val ready = poll(fds, n.convert(), 50)
                    if (ready < 0) {
                        if (errno == EINTR) continue
                        error("poll: ${lastError()}")
                    }
                    for (i in 0 until n) {
                        val revents = fds[i].revents.toInt()
                        if (revents == 0) continue
                        val fd = slots[i]
                        if (fd == inFd) {
                            writeInput()
                            continue
                        }
                        if (revents and (POLLIN or POLLHUP or POLLERR) != 0) {
                            val count =
                                buffer.usePinned { pinned -> read(fd, pinned.addressOf(0), CHUNK.convert()).toInt() }
                            when {
                                count > 0 -> {
                                    accept(if (fd == outFd) 1 else 2, buffer.copyOf(count))
                                }

                                count == 0 || (errno != EAGAIN && errno != EINTR) -> {
                                    if (fd == outFd) outOpen = false else errOpen = false
                                }
                            }
                        }
                    }
                    if (truncated) {
                        stop()
                        break
                    }
                }
            }
            close(outFd)
            close(errFd)
            if (inFd >= 0) close(inFd)
            if (status == null) {
                // The pipes closed or the child was told to stop: give it the grace period, then kill the group.
                val deadline = TimeSource.Monotonic.markNow()
                while (status == null && deadline.elapsedNow() < KILL_GRACE) {
                    reap(WNOHANG)
                    if (status == null) platform.posix.usleep(20_000u)
                }
                if (status == null) {
                    kill(-pid, SIGKILL)
                    reap(0)
                }
            }
            // A child whose end couldn't be learnt did not succeed.
            val s = status ?: return ProcResult(-1, null, out.bytes(), err.bytes(), timedOut, truncated)
            val signal = if (s and 0x7f != 0) s and 0x7f else null
            val code = if (signal == null) (s shr 8) and 0xff else -1
            return ProcResult(code, signal, out.bytes(), err.bytes(), timedOut, truncated)
        }

        private fun writeInput() {
            val data = input ?: return
            if (inputOffset < data.size) {
                val written =
                    data.usePinned { pinned ->
                        write(inFd, pinned.addressOf(inputOffset), (data.size - inputOffset).convert()).toInt()
                    }
                if (written > 0) {
                    inputOffset += written
                } else if (errno != EAGAIN && errno != EINTR) {
                    // The child closed its stdin (EPIPE: SIGPIPE is ignored, see [ignoreSigpipe]); nothing to send.
                    inputOffset = data.size
                }
            }
            if (inputOffset >= data.size) {
                close(inFd)
                inFd = -1
            }
        }

        private fun accept(
            fd: Int,
            bytes: ByteArray,
        ) {
            if (onChunk != null) {
                onChunk.invoke(fd, bytes)
                return
            }
            val target = if (fd == 1) out else err
            if (out.size + err.size + bytes.size > maxOutput) {
                target.add(bytes.copyOf((maxOutput - out.size - err.size).coerceAtLeast(0)))
                truncated = true
            } else {
                target.add(bytes)
            }
        }

        private fun stop() {
            kill(-pid, SIGTERM)
        }

        private fun reap(flags: Int) {
            memScoped {
                val st = alloc<IntVar>()
                val r = waitpid(pid, st.ptr, flags)
                if (r == pid) status = st.value
            }
        }
    }

    private class Buffer {
        private val parts = mutableListOf<ByteArray>()
        var size = 0
            private set

        fun add(bytes: ByteArray) {
            parts += bytes
            size += bytes.size
        }

        fun bytes(): ByteArray {
            val all = ByteArray(size)
            var at = 0
            for (p in parts) {
                p.copyInto(all, at)
                at += p.size
            }
            return all
        }
    }

    private fun lastError(): String = strerror(errno)?.toKString() ?: "errno $errno"
}
