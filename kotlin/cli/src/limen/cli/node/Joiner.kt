package limen.cli.node

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import limen.cli.os.HttpException
import limen.cli.os.HttpLite
import limen.cli.os.Sys
import limen.core.LenientJson
import limen.core.WireJson
import limen.core.config.HubConfig
import limen.core.join.Arrival
import limen.core.join.Invitation
import limen.core.join.JoinUrl
import limen.core.join.Keys
import limen.core.join.Welcome

/**
 * `limen join` (spec §10.1): this node joins a hub. With a join line, the hub's key is downloaded and checked
 * against the fingerprint in the line —what stops anyone between the two from slipping in their own key—, the node
 * is installed with it, and the hub is told this node's host key. With `--hub-key` there is no hub to talk to: the
 * node is installed and prints the `limen trust` line for the hub.
 */
class Joiner(
    private val installer: Installer,
) {
    fun join(
        url: JoinUrl,
        deployKey: String?,
        from: String?,
        repo: ((String) -> RepoOptions)?,
        address: String?,
        sshPort: Int,
    ): Int {
        val invitation = fetch(url)
        val fingerprint = Keys.fingerprint(invitation.hubKey)
        if (fingerprint != url.fingerprint) {
            fail(
                "the hub's key ($fingerprint) is not the one the join line names (${url.fingerprint}): something between this " +
                    "machine and the hub changed it. Nothing was installed.",
            )
        }
        say("Joining the hub at ${url.base} as '${invitation.name}' (hub key $fingerprint)")
        installer.install(invitation.hubKey, deployKey, from, repo?.invoke(invitation.name), announce = false)
        val hostKey = installer.hostKey() ?: fail("cannot read this machine's SSH host key")
        val welcome = arrive(url, Arrival(Keys.withoutComment(hostKey), installer.readUser, sshPort, address).signed(url.secret))
        say("")
        return if (welcome.reachable) {
            say("${welcome.name} is on the hub, at ${welcome.address}: ${welcome.detail}.")
            0
        } else {
            say("${welcome.name} is on the hub at ${welcome.address}, but the hub can't reach it yet: ${welcome.detail}")
            say("Check that the hub reaches this machine's SSH port ($sshPort) at that address, or join again with --address.")
            1
        }
    }

    fun withKey(
        hubKey: String,
        name: String,
        deployKey: String?,
        from: String?,
        repo: RepoOptions?,
        sshPort: Int,
    ): Int {
        Keys.fingerprint(hubKey)
        installer.install(hubKey, deployKey, from, repo, announce = false)
        val hostKey = installer.hostKey()?.let(Keys::withoutComment) ?: fail("cannot read this machine's SSH host key")
        val extra =
            listOfNotNull(
                if (installer.readUser !=
                    "limen-read"
                ) {
                    "--user ${installer.readUser}"
                } else {
                    null
                },
                if (sshPort != 22) "--port $sshPort" else null,
            )
        say("")
        say("limen is installed. On the hub, with this machine's address:")
        say("  limen trust $name <address> '$hostKey'${extra.joinToString("") { " $it" }}")
        return 0
    }

    private fun fetch(url: JoinUrl): Invitation {
        val r = http("GET", url, null)
        if (r.status != 200) fail(errorOf(r.body) ?: "the hub answered HTTP ${r.status}")
        val invitation =
            runCatching {
                LenientJson.decodeFromString(Invitation.serializer(), r.body)
            }.getOrElse { fail("the hub's answer is not an invitation") }
        // The name becomes the repository folder in this node's limen.toml: from a hub, it is checked like any input.
        if (!HubConfig.NODE_NAME.matches(invitation.name)) fail("the hub named this machine '${invitation.name}', which is not a node name")
        return invitation
    }

    private fun arrive(
        url: JoinUrl,
        arrival: Arrival,
    ): Welcome {
        val r = http("POST", url, WireJson.encodeToString(Arrival.serializer(), arrival))
        if (r.status != 200) fail("installed, but the hub refused it: ${errorOf(r.body) ?: "HTTP ${r.status}"}")
        return runCatching {
            LenientJson.decodeFromString(
                Welcome.serializer(),
                r.body,
            )
        }.getOrElse { fail("the hub's answer is not a welcome") }
    }

    private fun http(
        method: String,
        url: JoinUrl,
        body: String?,
    ): HttpLite.Response =
        try {
            HttpLite.request(method, url.host, url.port, "/join/${url.code}", body)
        } catch (e: HttpException) {
            fail("cannot reach the hub at ${url.base}: ${e.message}")
        }

    private fun errorOf(body: String): String? =
        runCatching { (LenientJson.parseToJsonElement(body) as? JsonObject)?.get("error") as? JsonPrimitive }.getOrNull()?.contentOrNull

    private fun say(text: String) = Sys.out("$text\n")

    private fun fail(message: String): Nothing = throw Installer.InstallException(message)
}
