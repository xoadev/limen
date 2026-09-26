package limen.core

import kotlinx.serialization.json.JsonPrimitive
import limen.core.scripts.FileStat
import limen.core.scripts.HeaderException
import limen.core.scripts.ScriptHeaders
import limen.core.scripts.ScriptKind
import limen.core.scripts.Trust
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ScriptsTest {
    private val script =
        """
        #!/usr/bin/env bash
        #: description = "Free space on the backup volume"
        #: timeout = "30s"
        # an ordinary comment in between
        #: [args.threshold]
        #: type = "int"
        #: default = 90
        #: range = [1, 100]
        #: description = "Percent above which it warns"
        #: [args.mount]
        #: type = "string"
        set -euo pipefail
        #: description = "not part of the header"
        """.trimIndent()

    @Test
    fun parsesTheHeader() {
        val spec = ScriptHeaders.parse("backup-space", ScriptKind.CHECK, script)
        assertEquals("Free space on the backup volume", spec.description)
        assertEquals(30, spec.timeoutSeconds)
        val threshold = spec.params.first { it.name == "threshold" }
        assertEquals(JsonPrimitive(90L), threshold.default)
        assertEquals(1L, threshold.min)
        assertEquals(false, threshold.required)
        val mount = spec.params.first { it.name == "mount" }
        assertTrue(mount.required)
        assertEquals("^[A-Za-z0-9._-]{1,64}$", mount.pattern)
    }

    @Test
    fun defaultTimeoutByKind() {
        val text = "#!/bin/sh\n#: description = \"x\"\n"
        assertEquals(60, ScriptHeaders.parse("a", ScriptKind.CHECK, text).timeoutSeconds)
        assertEquals(3600, ScriptHeaders.parse("a", ScriptKind.ACTION, text).timeoutSeconds)
    }

    @Test
    fun rejectsBrokenHeaders() {
        val cases =
            mapOf(
                "#!/bin/sh\necho hi" to "no `#:` header",
                "#: timeout = \"1s\"" to "no description",
                "#: description = \"x\"\n#: colour = 1" to "unknown key",
                "#: description = \"x\"\n#: [args.n]\n#: type = \"float\"" to "type 'float'",
                "#: description = \"x\"\n#: [args.n]\n#: type = \"enum\"" to "without values",
                "#: description = \"x\"\n#: [args.n]\n#: type = \"int\"\n#: range = [1, 5]\n#: default = 9" to "default does not fit",
                "#: description = \"x\"\n#: [args.Bad]\n#: type = \"int\"" to "argument name",
                "#: description = \"x\"\n#: timeout = \"soon\"" to "not a duration",
            )
        for ((text, expected) in cases) {
            val e = assertFailsWith<HeaderException>(text) { ScriptHeaders.parse("s", ScriptKind.CHECK, text) }
            assertTrue(expected in e.message!!, "${e.message} should mention '$expected'")
        }
    }

    @Test
    fun names() {
        assertEquals("disk-space", ScriptHeaders.nameOf("disk-space.sh", ScriptKind.CHECK))
        assertEquals("disk", ScriptHeaders.nameOf("disk", ScriptKind.CHECK))
        assertNull(ScriptHeaders.nameOf("Disk.sh", ScriptKind.CHECK))
        assertNull(ScriptHeaders.nameOf(".hidden", ScriptKind.CHECK))
        assertEquals("10-base", ScriptHeaders.nameOf("10-base.sh", ScriptKind.SETUP))
        assertNull(ScriptHeaders.nameOf("base.sh", ScriptKind.SETUP))
        assertEquals("LIMEN_ARG_THRESHOLD", ScriptHeaders.envName("threshold"))
    }

    @Test
    fun trustFollowsStrictModes() {
        fun stat(
            path: String,
            uid: Int = 0,
            mode: Int = 0b111_101_101,
            dir: Boolean = true,
        ) = FileStat(path, uid, mode, isDirectory = dir, isRegular = !dir)
        val script = stat("/etc/limen/checks.d/a", dir = false)
        val parents = listOf(stat("/etc/limen/checks.d"), stat("/etc/limen"), stat("/etc"), stat("/"))
        assertNull(Trust.problem(listOf(script) + parents, owner = 0))
        assertEquals(
            "/etc/limen/checks.d/a is not owned by root",
            Trust.problem(listOf(script.copy(uid = 1000)) + parents, 0),
        )
        // Run as a user, that user's files are trusted too, and root's parents still are.
        assertNull(Trust.problem(listOf(script.copy(uid = 1000)) + parents, 1000))
        assertEquals(
            "/etc/limen/checks.d/a is not owned by root or uid 1000",
            Trust.problem(listOf(script.copy(uid = 1001)) + parents, 1000),
        )
        assertEquals(
            "/etc/limen is writable by group or others",
            Trust.problem(listOf(script, parents[0], parents[1].copy(mode = 0b111_111_101)) + parents.drop(2), 0),
        )
        assertEquals(
            "/etc/limen/checks.d/a is not executable",
            Trust.problem(listOf(script.copy(mode = 0b110_100_100)) + parents, 0),
        )
    }
}
