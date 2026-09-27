package limen.core

import limen.core.files.Glob
import limen.core.files.PathPolicy
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue

class PathPolicyTest {
    @Test
    fun globSegments() {
        assertTrue(Glob("/etc/nginx/**").matches("/etc/nginx/sites/default"))
        assertTrue(Glob("/etc/nginx/**").matches("/etc/nginx"))
        assertFalse(Glob("/etc/nginx/**").matches("/etc/nginx2/x"))
        assertTrue(Glob("/opt/stacks/*/compose.yaml").matches("/opt/stacks/immich/compose.yaml"))
        assertFalse(Glob("/opt/stacks/*/compose.yaml").matches("/opt/stacks/a/b/compose.yaml"))
        assertTrue(Glob("/var/log/*.log").matches("/var/log/syslog.log"))
        assertFalse(Glob("/var/log/*.log").matches("/var/log/syslog"))
        assertTrue(Glob("**/*.env").matches("/opt/app/.env"))
        assertTrue(Glob("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key"))
        assertFalse(Glob("/etc/ssh/ssh_host_*_key").matches("/etc/ssh/ssh_host_ed25519_key.pub"))
        assertTrue(Glob("/a/?.txt").matches("/a/b.txt"))
    }

    @Test
    fun mayMatchBelowWalksTowardsAllowedFiles() {
        val g = Glob("/opt/stacks/*/compose.yaml")
        assertTrue(g.mayMatchBelow("/opt"))
        assertTrue(g.mayMatchBelow("/opt/stacks/immich"))
        assertFalse(g.mayMatchBelow("/srv"))
        assertTrue(Glob("/var/**/x").mayMatchBelow("/var/lib/a/b"))
    }

    @Test
    fun allowDenyAndBuiltIn() {
        val policy = PathPolicy(allow = listOf("/etc/**", "/opt/**"), deny = listOf("**/*.env"))
        assertEquals(PathPolicy.Decision.Allowed, policy.check("/etc/nginx/nginx.conf"))
        assertIs<PathPolicy.Decision.Denied>(policy.check("/opt/app/.env"))
        assertIs<PathPolicy.Decision.Denied>(policy.check("/srv/data"))
        for (secret in listOf(
            "/etc/shadow",
            "/etc/sudoers",
            "/etc/sudoers.tmp",
            "/etc/sudoers.d/limen",
            "/etc/dropbear/dropbear_ed25519_host_key",
            "/etc/ssh/ssh_host_rsa_key",
            "/home/ana/.ssh/id_ed25519",
            "/etc/ssl/private/key.pem",
            "/etc/wireguard/wg0.conf",
            "/etc/NetworkManager/system-connections/home.nmconnection",
            "/etc/limen/limen.toml",
        )) {
            assertIs<PathPolicy.Decision.Denied>(policy.check(secret), secret)
        }
        val everything = PathPolicy(allow = listOf("/**"), deny = emptyList())
        assertIs<PathPolicy.Decision.Denied>(everything.check("/proc/1/environ"))
        assertIs<PathPolicy.Decision.Denied>(everything.check("/dev/sda"))
        assertIs<PathPolicy.Decision.Denied>(everything.check("/root/.bash_history"))
        assertEquals(PathPolicy.Decision.Allowed, everything.check("/etc/dropbear/dropbear_ed25519_host_key.pub"))
    }

    @Test
    fun normalizeResolvesDotsAsText() {
        assertEquals("/etc/shadow", PathPolicy.normalize("/etc/nginx/../shadow"))
        assertEquals("/etc/shadow", PathPolicy.normalize("/../../etc/./shadow"))
        assertEquals("/", PathPolicy.normalize("/a/.."))
        assertEquals("/a/b", PathPolicy.normalize("//a//b/"))
    }

    @Test
    fun emptyAllowlistAllowsNothing() {
        val policy = PathPolicy(emptyList(), emptyList())
        assertIs<PathPolicy.Decision.Denied>(policy.check("/etc/hostname"))
        assertFalse(policy.leadsTo("/etc"))
    }

    @Test
    fun leadsToStopsAtDeniedDirectories() {
        val policy = PathPolicy(allow = listOf("/etc/**"), deny = emptyList())
        assertTrue(policy.leadsTo("/"))
        assertFalse(policy.leadsTo("/etc/ssl/private"))
    }
}
