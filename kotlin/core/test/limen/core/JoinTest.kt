package limen.core

import limen.core.config.HubConfig
import limen.core.join.HubFile
import limen.core.join.JoinUrl
import limen.core.join.Keys
import limen.core.join.Sha256
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class JoinTest {
    @Test
    fun sha256StandardVectors() {
        assertEquals("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", Sha256.hex(ByteArray(0)))
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", Sha256.hex("abc".encodeToByteArray()))
        assertEquals(
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            Sha256.hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".encodeToByteArray()),
        )
        // Across the 55/56-byte padding boundary and several blocks.
        assertEquals(
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            Sha256.hex(ByteArray(1_000_000) { 'a'.code.toByte() }),
        )
    }

    @Test
    fun fingerprintsMatchSshKeygen() {
        assertEquals(
            "SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU",
            Keys.fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6gEiSLgluUCGAAsH0PgwdjMmtbI2Ow7steqWQs2UQy test"),
        )
        assertEquals(
            "SHA256:wilATxNCJD61k0JDieHTGe0cizFZxKkoz+3UJYJhjkU",
            Keys.fingerprint(
                "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDM24zpXzxkBJklgbjA+7nyzbLUCV+RQCtU7dEyxhagLY+kyIBlbJgVzrfW4r/6nn7BMpfpH3r4NEmggvt/uZiLv8eLi2mle6SlGtJzicutHQdMpRKchNzyO2q34TRi8eOGLvv7fbGz7dORtPYYFwyVbOVSoDyigU5Ftk6dugTGCBEwEI3oaU+8LwxJQ9Ua1/g1SY9IFhkh5M6S8K7sXtbbWYYpHjEDtWpg6Rpigpy3R5btE6v633RdD1VPuWxWB9REikjlSGpryUd++ICpXsg/fnnLgMtUBORbR5WH4oFMeRFjZ5fAx59HTiO6em9KoBbPRSMtZZvjSXXE37fiBA7l",
            ),
        )
        assertFailsWith<LimenException> { Keys.fingerprint("not a key") }
    }

    @Test
    fun joinUrls() {
        val url = JoinUrl.parse("http://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU")
        assertEquals("http://100.64.0.2:7341", url.base)
        assertEquals("100.64.0.2", url.host)
        assertEquals(7341, url.port)
        assertEquals("abcdefghijklmnopqrstuvwxyz", url.code)
        for (bad in listOf(
            "http://hub.lan:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU",
            "http://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz",
            "https://100.64.0.2:7341/join/abcdefghijklmnopqrstuvwxyz#SHA256:/e0jvmq8w0ulx75514RErZFch0647RozrdByE6ZZjnU",
        )) {
            assertFailsWith<LimenException>(bad) { JoinUrl.parse(bad) }
        }
    }

    @Test
    fun hubFileReplacesOnlyTheNode() {
        val text =
            """
            # my hub
            [ssh]
            identity = "id_ed25519"

            [nodes.nas]
            host = "10.0.0.1"
            host_key = "ssh-ed25519 AAAAold"

            [nodes.router]
            host = "10.0.0.254"
            user = "root"
            host_key = "ssh-ed25519 AAAArouter"
            """.trimIndent()
        val updated = HubFile.upsertNode(text, "nas", "10.0.0.2", 22, "limen-read", "ssh-ed25519 AAAAnew nas@host")
        val config = HubConfig.parse(updated)
        assertEquals(listOf("router", "nas"), config.nodes.map { it.name })
        assertEquals("10.0.0.2", config.node("nas")!!.host)
        assertEquals("ssh-ed25519 AAAAnew", config.node("nas")!!.hostKey)
        assertEquals("root", config.node("router")!!.user)
        assertEquals(true, updated.startsWith("# my hub\n[ssh]"))
        val fresh = HubConfig.parse(HubFile.upsertNode("", "caronte", "10.0.0.9", 2222, "root", "ssh-ed25519 AAAAx"))
        assertEquals(2222, fresh.node("caronte")!!.port)
        assertEquals(HubConfig.parse(text).nodes.size - 1, HubConfig.parse(HubFile.removeNode(text, "nas")).nodes.size)
    }

    @Test
    fun publicUrlIsAnAddress() {
        assertEquals("http://100.64.0.2:7341", HubConfig.parse("[http]\npublic_url = \"http://100.64.0.2:7341/\"").publicUrl)
        assertFailsWith<limen.core.toml.TomlException> { HubConfig.parse("[http]\npublic_url = \"http://hub.lan:7341\"") }
    }
}
