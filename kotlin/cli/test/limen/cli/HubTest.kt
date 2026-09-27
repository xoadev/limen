package limen.cli

import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import limen.cli.hub.Hub
import limen.cli.hub.NodeClient
import limen.cli.os.Fs
import limen.cli.os.HttpLite
import limen.cli.os.Proc
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.NodeResponse
import limen.core.config.HubConfig
import limen.core.join.Arrival
import limen.core.join.Keys
import platform.posix.setenv
import platform.posix.unsetenv
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.seconds

class HubTest {
    private val dirs = mutableListOf<String>()

    private fun hub(): Hub {
        val dir = Proc.run(listOf("/bin/mktemp", "-d")).out.trim()
        dirs += dir
        return Hub("$dir/hub")
    }

    @AfterTest
    fun removeTheHubs() {
        dirs.forEach { Proc.run(listOf("/bin/rm", "-rf", it)) }
    }

    private class Answering : NodeClient {
        override val nodes = listOf("nas")

        override suspend fun call(
            node: String,
            request: String,
            args: JsonObject,
        ) = NodeResponse.success(
            buildJsonObject {
                put("os", "Debian GNU/Linux 13")
                put("version", "0.1.0")
            },
        )
    }

    @Test
    fun initCreatesWhatIsMissingOnly() {
        val hub = hub()
        val created = hub.init(serve = true)
        assertEquals(listOf(hub.keyPath, hub.configPath, hub.tokenPath), created)
        assertTrue(Keys.fingerprint(hub.publicKey).startsWith("SHA256:"))
        assertEquals(32, hub.token().length)
        assertEquals(emptyList(), hub.init(serve = true))
        assertEquals(emptyList(), hub.config().nodes)
    }

    @Test
    fun anInvitationJoinsOnceWithTheAddressItCameFrom() {
        val hub = hub()
        hub.init(serve = false)
        val code = hub.invite("nas")
        assertEquals("nas", hub.invitation(code)!!.name)
        val hostKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy root@nas"
        val welcome = runBlocking { hub.arrive(code, Arrival(hostKey, "limen-read"), "::ffff:10.0.0.7", Answering()) }
        assertTrue(welcome.reachable)
        assertEquals("10.0.0.7", welcome.address)
        assertEquals("Debian GNU/Linux 13, limen 0.1.0", welcome.detail)
        val node = hub.config().node("nas")!!
        assertEquals("10.0.0.7", node.host)
        assertEquals(Keys.withoutComment(hostKey), node.hostKey)
        assertNull(hub.invitation(code))
        assertFailsWith<LimenException> { runBlocking { hub.arrive(code, Arrival(hostKey, "limen-read"), "10.0.0.8", Answering()) } }
    }

    @Test
    fun invitationsExpireAndCodesAreChecked() {
        val hub = hub()
        hub.init(serve = false)
        val code = hub.invite("router", ttl = (-1).seconds)
        assertNull(hub.invitation(code))
        assertNull(hub.invitation("../../etc/passwd"))
        assertFailsWith<LimenException> { hub.invite("Not A Name") }
    }

    @Test
    fun aBadHostKeyNeverReachesTheFile() {
        val hub = hub()
        hub.init(serve = false)
        val code = hub.invite("nas")
        assertFailsWith<LimenException> {
            runBlocking {
                hub.arrive(
                    code,
                    Arrival("ssh-ed25519 \"; rm", "limen-read"),
                    "10.0.0.7",
                    Answering(),
                )
            }
        }
        assertEquals(emptyList(), HubConfig.parse(Fs.readText(hub.configPath)!!).nodes)
        assertNotNull(hub.invitation(code))
    }

    @Test
    fun anArrivalCanOnlyAddTheNodeItWasInvitedAs() {
        val hub = hub()
        hub.init(serve = false)
        val before = Fs.readText(hub.configPath)!!
        val key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy"
        val injections =
            listOf(
                Arrival(key, "limen-read", address = "10.0.0.7\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"$key"),
                Arrival(key, "limen-read", address = "10.0.0.7\"\n[http]\norigins = [\"http://evil\"]\n#"),
                Arrival(key, "root\"\n[nodes.evil]\nhost = \"6.6.6.6\"\nhost_key = \"$key\"\n#", address = "10.0.0.7"),
                Arrival("$key\"\n[http]\nlisten = \"0.0.0.0:1\"\n#", "limen-read", address = "10.0.0.7"),
                // Well formed: the hub's own `host_key` line completes the injected node.
                Arrival(key, "limen-read", address = "10.0.0.7\"\nhost_key = \"$key\"\n[nodes.evil]\nhost = \"6.6.6.6"),
            )
        for (arrival in injections) {
            val code = hub.invite("nas")
            assertFailsWith<LimenException>(arrival.toString()) { runBlocking { hub.arrive(code, arrival, "10.0.0.7", Answering()) } }
            assertEquals(before, Fs.readText(hub.configPath), "limen.toml changed by $arrival")
        }
    }

    @Test
    fun aShortTokenIsRefused() {
        val hub = hub()
        hub.init(serve = true)
        setenv("LIMEN_TOKEN", "short", 1)
        try {
            assertEquals(ErrorCode.BAD_REQUEST, assertFailsWith<LimenException> { hub.token() }.code)
            setenv("LIMEN_TOKEN", "a".repeat(Hub.MIN_TOKEN), 1)
            assertEquals("a".repeat(Hub.MIN_TOKEN), hub.token())
        } finally {
            unsetenv("LIMEN_TOKEN")
        }
    }

    @Test
    fun httpAnswersPlainAndChunked() {
        val plain = HttpLite.parse("HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}".encodeToByteArray())
        assertEquals(404, plain.status)
        assertEquals("{}", plain.body)
        val chunked =
            HttpLite.parse(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n5\r\n\"ñ\"}\r\n0\r\n\r\n".encodeToByteArray(),
            )
        assertEquals("{\"a\":\"ñ\"}", chunked.body)
    }
}
