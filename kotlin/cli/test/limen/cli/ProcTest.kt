package limen.cli

import limen.cli.os.Fs
import limen.cli.os.Proc
import platform.posix.usleep
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.seconds
import kotlin.time.TimeSource

class ProcTest {
    private val sh = "/bin/sh"

    @Test
    fun separatesStreamsAndFeedsStdin() {
        val r = Proc.run(listOf(sh, "-c", "cat; echo err >&2; exit 3"), stdin = "hello\n".encodeToByteArray())
        assertEquals(3, r.exitCode)
        assertEquals("hello\n", r.out)
        assertEquals("err\n", r.err)
    }

    @Test
    fun theEnvironmentIsExactlyTheOneGiven() {
        val r = Proc.run(listOf(sh, "-c", "env"), env = listOf("ONLY=this"))
        assertTrue("ONLY=this" in r.out)
        assertFalse("HOME=" in r.out)
    }

    @Test
    fun argumentsNeverReachAShell() {
        val r = Proc.run(listOf("/bin/echo", "a; echo injected", "\$(id)"))
        assertEquals("a; echo injected \$(id)\n", r.out)
    }

    @Test
    fun aTimeoutStopsTheWholeGroup() {
        val start = TimeSource.Monotonic.markNow()
        val r = Proc.run(listOf(sh, "-c", "sleep 30 & echo \$!; sleep 30"), timeout = 1.seconds)
        assertTrue(r.timedOut)
        assertTrue(start.elapsedNow() < 10.seconds, "took ${start.elapsedNow()}")
        // The grandchild too, not only the shell that started it: gone, or a zombie waiting for its new parent.
        val grandchild = r.out.trim()
        val alive = {
            Fs
                .readText("/proc/$grandchild/stat")
                ?.substringAfterLast(") ")
                ?.firstOrNull()
                ?.let { it != 'Z' } ?: false
        }
        val deadline = TimeSource.Monotonic.markNow() + 2.seconds
        while (alive() && deadline.hasNotPassedNow()) usleep(50_000u)
        assertFalse(alive(), "sleep $grandchild outlived the timeout")
    }

    @Test
    fun outputOverTheCapIsCut() {
        val r = Proc.run(listOf(sh, "-c", "while :; do echo xxxxxxxxxxxxxxxx; done"), maxOutput = 10_000, timeout = 10.seconds)
        assertTrue(r.truncated)
        assertFalse(r.timedOut)
        assertTrue(r.stdout.size <= 10_000)
    }

    @Test
    fun streamsWhenAsked() {
        val chunks = mutableListOf<Pair<Int, String>>()
        val r = Proc.run(listOf(sh, "-c", "echo one; echo two >&2")) { fd, bytes -> chunks += fd to bytes.decodeToString() }
        assertEquals(0, r.exitCode)
        assertEquals(setOf(1 to "one\n", 2 to "two\n"), chunks.toSet())
        assertEquals(0, r.stdout.size)
    }

    @Test
    fun aChildThatIgnoresStdinDoesNotKillUs() {
        val r = Proc.run(listOf("/bin/true"), stdin = ByteArray(1_000_000) { 'x'.code.toByte() })
        assertEquals(0, r.exitCode)
    }

    @Test
    fun stdinIsDevNullEveryTime() {
        // Regression: the static glibc kept a pointer to a freed path and the child opened garbage (exit 127).
        repeat(50) {
            val r = Proc.run(listOf(sh, "-c", "read x; echo eof:\$?"))
            assertEquals("eof:1\n", r.out, "run $it: exit ${r.exitCode}")
        }
    }

    @Test
    fun whichFindsInTheFixedPath() {
        assertEquals("/bin/sh", Proc.which("/bin/sh"))
        assertTrue(Proc.which("sh")!!.endsWith("/sh"))
        assertEquals(null, Proc.which("no-such-program-limen"))
    }
}
