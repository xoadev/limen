package limen.core

import limen.core.config.Expectations
import limen.core.config.GitHub
import limen.core.config.NodeConfig
import limen.core.system.ProcFs
import limen.core.toml.TomlException
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ProcFsTest {
    @Test
    fun accountsAndGroups() {
        val passwd = "root:x:0:0:root:/root:/bin/ash\n# comment\nlimen-read:x:998:998::/var/lib/limen-read:/bin/sh\nbroken\n"
        val accounts = ProcFs.accounts(passwd)
        assertEquals(listOf("root", "limen-read"), accounts.map { it.name })
        assertEquals("/bin/sh", accounts[1].shell)
        assertEquals(mapOf(0 to "root", 33 to "www-data"), ProcFs.groups("root:x:0:\nwww-data:x:33:\n"))
    }

    @Test
    fun mountsKeepDataFilesystemsOnce() {
        val text =
            """
            proc /proc proc rw 0 0
            /dev/root /rom squashfs ro 0 0
            overlayfs:/overlay / overlay rw 0 0
            /dev/ubi0_1 /overlay ubifs rw 0 0
            tmpfs /tmp tmpfs rw 0 0
            /dev/nvme0n1p2 / ext4 rw 0 0
            /dev/nvme0n1p2 /var/lib/docker ext4 rw 0 0
            overlay /var/lib/docker/overlay2/abc/merged overlay rw 0 0
            /dev/sda1 /mnt/my\040disk ext4 rw 0 0
            """.trimIndent()
        // One entry per mount point and per device: no pseudo-filesystems, no container layers, no bind mounts.
        assertEquals(listOf("/", "/overlay", "/mnt/my disk"), ProcFs.mounts(text).map { it.point })
    }

    @Test
    fun sockets() {
        val tcp =
            """
            sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
             0: 0100007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   101        0 23456 1
             1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 34567 1
            """.trimIndent()
        val sockets = ProcFs.sockets(tcp, "tcp")
        assertEquals("127.0.0.1", sockets[0].address)
        assertEquals(53, sockets[0].port)
        assertTrue(sockets[0].listening)
        assertTrue(!sockets[1].listening)
        assertEquals(23456, sockets[0].inode)
        assertEquals("::", ProcFs.address("00000000000000000000000000000000"))
        assertEquals("::1", ProcFs.address("00000000000000000000000001000000"))
        assertEquals("fe80::1", ProcFs.address("000080FE000000000000000001000000"))
    }

    @Test
    fun processSample() {
        val stat = "1234 (my (odd) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 5000 1000 200 18446744073709551615"
        val status = "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t   2048 kB\n"
        val p = ProcFs.sample(1234, stat, status, "/usr/bin/app\u0000--flag\u0000")!!
        assertEquals("my (odd) name", p.comm)
        assertEquals(1000, p.uid)
        assertEquals(250, p.utimeTicks)
        assertEquals(50, p.stimeTicks)
        assertEquals(5000, p.startTicks)
        assertEquals(2048, p.rssKb)
        assertEquals("/usr/bin/app --flag", p.cmdline)
        // 3 s of CPU over the 100 s since it started (at 50 s of a 150 s uptime).
        assertEquals(3.0, ProcFs.cpuPercent(p, 150.0, 100))
        assertEquals("[kthreadd]", ProcFs.sample(2, "2 (kthreadd) S 0 0 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1 0 0", "Uid:\t0", "")!!.cmdline)
    }

    @Test
    fun logread() {
        val e = ProcFs.logreadLine("Sat Sep 26 07:05:09 2026 daemon.warn dnsmasq[1234]: no servers found")!!
        assertEquals("2026-09-26T07:05:09Z", e.time)
        assertEquals("warning", e.priority)
        assertEquals("dnsmasq", e.source)
        assertEquals(1234, e.pid)
        assertEquals("no servers found", e.message)
        val k = ProcFs.logreadLine("Mon Jan  5 10:00:00 2026 kern.info kernel: [    1.234] usb 1-1: new device")!!
        assertEquals("kernel", k.source)
        assertNull(k.pid)
        assertEquals("2026-01-05T10:00:00Z", k.time)
        assertNull(ProcFs.logreadLine("not a log line"))
    }

    @Test
    fun repoConfigDerivesTheScriptDirectories() {
        val config =
            NodeConfig.parse(
                """
                [repo]
                url = "https://github.com/you/infra.git"
                path = "nodes/nas"
                [scripts]
                checks = "/etc/limen/checks.d"
                """.trimIndent(),
            )
        val repo = config.repo!!
        assertEquals("main", repo.branch)
        assertEquals("/opt/limen/repo/nodes/nas/setup", config.setup)
        assertEquals("/etc/limen/checks.d", config.checks)
        assertEquals("/opt/limen/repo/nodes/nas/stacks", config.stacks)
        assertEquals("/opt/limen/last-sync.json", repo.syncRecord)
        assertEquals("you" to "infra", repo.github)
        assertEquals("https://github.com/x/y", NodeConfig.parse("[repo]\nurl = \"https://u:p@github.com/x/y\"").repo!!.displayUrl)
        for (bad in listOf(
            "url = \"http://x\"",
            "url = \"https://x\"\npath = \"../etc\"",
            "url = \"https://x\"\ndir = \"/opt\"",
            "url = \"https://x\"\nbranch = \"-x\"",
        )) {
            assertFailsWith<TomlException>(bad) { NodeConfig.parse("[repo]\n$bad") }
        }
    }

    @Test
    fun expectations() {
        val e = Expectations.parse("[expect]\ncompose = [\"immich\"]\nunits = [\"docker.service\"]\nprocd = [\"dnsmasq\"]")
        assertEquals(listOf("immich"), e.compose)
        assertEquals(listOf("docker.service"), e.units)
        assertTrue(Expectations.parse("").isEmpty)
        assertFailsWith<TomlException> { Expectations.parse("[expect]\ncompose = [\"../x\"]") }
        assertFailsWith<TomlException> { Expectations.parse("[expect]\nservices = []") }
    }

    @Test
    fun tokenLink() {
        assertEquals(
            "https://github.com/settings/personal-access-tokens/new?name=limen-nas" +
                "&description=limen%20on%20nas%3A%20read%20you%2Finfra&target_name=you&expires_in=none&contents=read",
            GitHub.tokenUrl("you", "infra", "nas"),
        )
    }
}
