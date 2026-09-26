package limen.core

import limen.core.config.HubConfig
import limen.core.config.NodeConfig
import limen.core.toml.TomlException
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.seconds

class ConfigTest {
    @Test
    fun nodeDefaultsAndOverrides() {
        assertEquals(NodeConfig(), NodeConfig.parse(""))
        val config =
            NodeConfig.parse(
                """
                [files]
                allow = ["/etc/nginx/**"]
                max_bytes = 1024
                [logs]
                max_lines = 10
                [scripts]
                checks = "/opt/cloud/ops/checks"
                """.trimIndent(),
            )
        assertEquals(listOf("/etc/nginx/**"), config.allow)
        assertEquals(1024, config.maxFileBytes)
        assertEquals(10, config.maxLines)
        assertEquals("/opt/cloud/ops/checks", config.checks)
        assertEquals("/etc/limen/actions.d", config.actions)
    }

    @Test
    fun nodeRejectsMistakes() {
        for ((text, expected) in mapOf(
            "[files]\nalow = []" to "files.alow: unknown key",
            "[files]\nallow = [\"etc/*\"]" to "files.allow",
            "[scripts]\nchecks = \"relative\"" to "scripts.checks: must be an absolute path",
            "[redact]\npatterns = [\"(\"]" to "redact.patterns: bad regex",
            "[logs]\nmax_lines = 0" to "logs.max_lines: must be positive",
        )) {
            val e = assertFailsWith<TomlException>(text) { NodeConfig.parse(text) }
            assertTrue(e.message!!.startsWith(expected), "${e.message} should start with $expected")
        }
    }

    @Test
    fun hub() {
        val config =
            HubConfig.parse(
                """
                [ssh]
                identity = "keys/limen"
                connect_timeout = "3s"

                [nodes.hades]
                host = "100.64.0.2"
                host_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx"

                [nodes.persephone]
                host = "persephone.lan"
                port = 2222
                user = "reader"
                host_key = "ecdsa-sha2-nistp256 AAAAE2VjZHNh="
                """.trimIndent(),
            )
        assertEquals("keys/limen", config.identity)
        assertEquals(3.seconds, config.connectTimeout)
        assertEquals(listOf("hades", "persephone"), config.nodes.map { it.name })
        assertEquals(22, config.node("hades")!!.port)
        assertEquals("limen-read", config.node("hades")!!.user)
        assertEquals("reader", config.node("persephone")!!.user)
        assertEquals("127.0.0.1", config.listenHost)
        assertEquals(7341, config.listenPort)
    }

    @Test
    fun hubRequiresAPinnedHostKey() {
        val e = assertFailsWith<TomlException> { HubConfig.parse("[nodes.hades]\nhost = \"h\"") }
        assertTrue("nodes.hades.host_key: missing" in e.message!!, e.message)
        val bad = assertFailsWith<TomlException> { HubConfig.parse("[nodes.hades]\nhost = \"h\"\nhost_key = \"h ssh-ed25519 AAAA\"") }
        assertTrue("host_key" in bad.message!!, bad.message)
        assertFailsWith<TomlException> { HubConfig.parse("[nodes.Hades]\nhost = \"h\"\nhost_key = \"ssh-ed25519 AAAA\"") }
    }
}
