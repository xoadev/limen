package limen.core

import limen.core.toml.Toml
import limen.core.toml.TomlArray
import limen.core.toml.TomlBool
import limen.core.toml.TomlException
import limen.core.toml.TomlInt
import limen.core.toml.TomlReader
import limen.core.toml.TomlString
import limen.core.toml.TomlTable
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class TomlTest {
    @Test
    fun tablesKeysAndValues() {
        val root =
            Toml.parse(
                """
                # comment
                top = "a # not a comment"

                [files]
                allow = [
                  "/etc/nginx/**",   # trailing comment
                  # "/commented/out",
                  '/opt/*/compose.yaml',
                ]
                max_bytes = 262_144
                enabled = true

                [nodes.nas]
                host = "100.64.0.2"
                "quoted key" = -3
                a.b = "dotted"
                """.trimIndent(),
            )
        assertEquals(TomlString("a # not a comment"), root.entries["top"])
        val files = root.entries["files"] as TomlTable
        assertEquals(TomlArray(listOf(TomlString("/etc/nginx/**"), TomlString("/opt/*/compose.yaml"))), files.entries["allow"])
        assertEquals(TomlInt(262144), files.entries["max_bytes"])
        assertEquals(TomlBool(true), files.entries["enabled"])
        val nas = (root.entries["nodes"] as TomlTable).entries["nas"] as TomlTable
        assertEquals(TomlInt(-3), nas.entries["quoted key"])
        assertEquals(TomlString("dotted"), (nas.entries["a"] as TomlTable).entries["b"])
    }

    @Test
    fun escapes() {
        val root = Toml.parse("""s = "tab\tquote\"slash\\ é"""")
        assertEquals("tab\tquote\"slash\\ é", (root.entries["s"] as TomlString).value)
    }

    @Test
    fun rejectsWhatItDoesNotSupport() {
        for (text in listOf("a = 1.5", "a = { b = 1 }", "[[t]]", "a = \"\"\"x\"\"\"", "a = 1\na = 2", "[t]\n[t]", "a = \"open")) {
            assertFailsWith<TomlException>(text) { Toml.parse(text) }
        }
    }

    @Test
    fun errorsNameTheLine() {
        val e = assertFailsWith<TomlException> { Toml.parse("a = 1\n\nb = ?") }
        assertTrue(e.message!!.startsWith("line 3:"), e.message)
    }

    @Test
    fun readerNamesTheKeyAndRejectsUnknownOnes() {
        val reader = TomlReader(Toml.parse("[files]\nallow = [1]\ntypo = 2"))
        val files = reader.table("files")!!
        val wrongType = assertFailsWith<TomlException> { files.strings("allow") }
        assertEquals("files.allow: expected an array of strings", wrongType.message)
        val unknown = assertFailsWith<TomlException> { files.rejectUnknown() }
        assertEquals("files.typo: unknown key", unknown.message)
    }
}
