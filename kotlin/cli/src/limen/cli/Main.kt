package limen.cli

import com.github.ajalt.clikt.core.CliktError
import com.github.ajalt.clikt.core.Context
import com.github.ajalt.clikt.core.CoreCliktCommand
import com.github.ajalt.clikt.core.PrintHelpMessage
import com.github.ajalt.clikt.core.PrintMessage
import com.github.ajalt.clikt.core.ProgramResult
import com.github.ajalt.clikt.core.UsageError
import com.github.ajalt.clikt.core.parse
import com.github.ajalt.clikt.core.subcommands
import com.github.ajalt.clikt.output.Localization
import com.github.ajalt.clikt.output.ParameterFormatter
import com.github.ajalt.clikt.parameters.arguments.argument
import com.github.ajalt.clikt.parameters.arguments.optional
import com.github.ajalt.clikt.parameters.options.default
import com.github.ajalt.clikt.parameters.options.flag
import com.github.ajalt.clikt.parameters.options.help
import com.github.ajalt.clikt.parameters.options.multiple
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import com.github.ajalt.clikt.parameters.options.versionOption
import com.github.ajalt.clikt.parameters.types.choice
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import limen.cli.hub.Http
import limen.cli.hub.Hub
import limen.cli.hub.LiveHub
import limen.cli.hub.Stdio
import limen.cli.node.Deploy
import limen.cli.node.Gate
import limen.cli.node.Installer
import limen.cli.node.Joiner
import limen.cli.node.Lint
import limen.cli.node.Node
import limen.cli.node.Read
import limen.cli.node.RepoOptions
import limen.cli.node.Scripts
import limen.cli.os.Proc
import limen.cli.os.Sys
import limen.core.Args
import limen.core.Durations
import limen.core.ErrorCode
import limen.core.LIMEN_BUILD_DATE
import limen.core.LIMEN_BUILD_NUMBER
import limen.core.LIMEN_VERSION
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.NodeRequest
import limen.core.NodeResponse
import limen.core.Param
import limen.core.ParamType
import limen.core.PrettyJson
import limen.core.Requests
import limen.core.Role
import limen.core.WireJson
import limen.core.config.HubConfig
import limen.core.config.NodeConfig
import limen.core.join.JoinUrl
import limen.core.join.Keys
import limen.core.scripts.Catalog
import limen.core.scripts.ScriptKind
import kotlin.system.exitProcess
import kotlin.time.Duration.Companion.hours

/** Entry point of the `limen` binary (spec §10). Exit codes: 0 ok, 1 error, 2 usage; `check` exits with the check's. */
fun main(args: Array<String>) {
    Proc.ignoreSigpipe()
    val code = Cli.run(args.toList())
    if (code != 0) exitProcess(code)
}

object Cli {
    fun run(args: List<String>): Int {
        val root =
            LimenCommand().subcommands(
                InitCommand(),
                McpCommand(),
                ServeCommand(),
                ConnectCommand(),
                InviteCommand(),
                TrustCommand(),
                ForgetCommand(),
                CallCommand(),
                JoinCommand(),
                GateCommand(),
                InstallCommand(),
                UninstallCommand(),
                SyncCommand(),
                ApplyCommand(),
                ActionCommand(),
                CheckCommand(),
                LintCommand(),
                TokenCommand(),
                VersionCommand(),
            )
        return try {
            root.parse(args)
            0
        } catch (e: ExitWith) {
            e.code
        } catch (e: PrintHelpMessage) {
            e.context
                ?.command
                ?.getFormattedHelp(e)
                ?.let { Sys.out(it + "\n") }
            if (e.error) 2 else 0
        } catch (e: PrintMessage) {
            e.message?.let { if (e.printError) Sys.err(it + "\n") else Sys.out(it + "\n") }
            e.statusCode
        } catch (e: ProgramResult) {
            e.statusCode
        } catch (e: UsageError) {
            Sys.err((e.context?.command?.getFormattedHelp(e) ?: e.formatMessage(object : Localization {}, ParameterFormatter.Plain)) + "\n")
            2
        } catch (e: CliktError) {
            Sys.err("limen: ${e.message}\n")
            if (e.statusCode == 0) 1 else e.statusCode
        } catch (e: LimenException) {
            Sys.err("limen: ${e.code.wire}: ${e.message}\n")
            1
        } catch (e: Exception) {
            Sys.err("limen: ${e.message ?: e::class.simpleName}\n")
            1
        }
    }
}

class ExitWith(
    val code: Int,
) : Exception()

/** `--arg key=value`, typed by [params] where it names one; otherwise a number, a boolean or a string. */
fun parseArgs(
    pairs: List<String>,
    params: List<Param>,
): JsonObject =
    JsonObject(
        pairs.associate { pair ->
            val key = pair.substringBefore('=', missingDelimiterValue = "")
            if (key.isEmpty()) throw UsageError("--arg takes key=value, not '$pair'")
            val value = pair.substringAfter('=')
            val type = params.firstOrNull { it.name == key }?.type
            key to
                when {
                    type == ParamType.INT || (type == null && value.toLongOrNull() != null) -> {
                        JsonPrimitive(value.toLongOrNull() ?: throw UsageError("$key must be an integer"))
                    }

                    type == ParamType.BOOL || (type == null && (value == "true" || value == "false")) -> {
                        JsonPrimitive(value.toBooleanStrictOrNull() ?: throw UsageError("$key must be true or false"))
                    }

                    else -> {
                        JsonPrimitive(value)
                    }
                }
        },
    )

private class LimenCommand : CoreCliktCommand("limen") {
    override fun help(context: Context) =
        "Read-only access to Linux machines for MCP clients. The hub runs `mcp` or `serve`; each node runs " +
            "`gate` as an SSH forced command, set up by `join` (or `install`)."

    init {
        versionOption(LIMEN_VERSION, message = { versionLine() })
    }

    override fun run() = Unit
}

private class McpCommand : CoreCliktCommand("mcp") {
    override fun help(context: Context) = "MCP server over stdio (the hub)"

    val home by option("--home").help("Hub directory with limen.toml (default: \$LIMEN_HOME or ~/.limen)")

    override fun run() {
        Stdio.run(LiveHub(Hub.at(home)))
    }
}

private class ServeCommand : CoreCliktCommand("serve") {
    override fun help(context: Context) = "MCP server over HTTP (the hub), and where nodes join; creates the hub on first start"

    val home by option("--home").help("Hub directory (default: \$LIMEN_HOME or ~/.limen)")
    val listen by option("--listen").help("host:port, over [http].listen and LIMEN_LISTEN")

    override fun run() {
        val hub = Hub.at(home)
        hub.init(serve = true).forEach { Sys.err("limen: created $it\n") }
        val token = hub.token()
        if (token.length < 16) throw LimenException(ErrorCode.BAD_REQUEST, "the token must have 16 characters or more")
        val live = LiveHub(hub)
        val address = listen ?: Sys.env("LIMEN_LISTEN")?.takeIf { it.isNotBlank() } ?: live.config().listen
        Http.run(hub, live, address, token)
    }
}

private class InitCommand : CoreCliktCommand("init") {
    override fun help(context: Context) = "Create the hub: its key, limen.toml and, with --serve, the token of HTTP clients"

    val home by option("--home").help("Hub directory (default: \$LIMEN_HOME or ~/.limen)")
    val serve by option("--serve").flag().help("Also the token for `limen serve`")

    override fun run() {
        val hub = Hub.at(home)
        val created = hub.init(serve)
        created.forEach { Sys.out("created $it\n") }
        if (created.isEmpty()) Sys.out("${hub.home} was already a hub\n")
        Sys.out("hub key: ${Keys.fingerprint(hub.publicKey)}\n")
        Sys.out("Next: `limen invite <name>` for each machine, and `limen connect` for the MCP client.\n")
    }
}

private class ConnectCommand : CoreCliktCommand("connect") {
    override fun help(context: Context) = "Print the command that connects Claude Code to this hub"

    val home by option("--home").help("Hub directory")
    val url by option("--url").help("The hub's HTTP address, over [http].public_url and LIMEN_PUBLIC_URL")

    override fun run() {
        val hub = Hub.at(home)
        val base = url ?: runCatching { hub.config().publicUrl }.getOrNull()
        val token = runCatching { hub.token() }.getOrNull()
        if (base != null && token != null) {
            Sys.out("claude mcp add --transport http limen $base/mcp --header \"Authorization: Bearer $token\"\n")
        } else {
            val homeOption = if (hub.home == Hub.home(null).trimEnd('/')) "" else " --home ${hub.home}"
            Sys.out("claude mcp add limen -- limen mcp$homeOption\n")
        }
    }
}

private class InviteCommand : CoreCliktCommand("invite") {
    override fun help(context: Context) = "A one-time line that joins a machine to this hub; paste it on the machine as root"

    val name by argument(help = "The machine's name on the hub")
    val home by option("--home").help("Hub directory")
    val ttl by option("--ttl").default("1h").help("How long the invitation lasts")

    override fun run() {
        val hub = Hub.at(home)
        if (!HubConfig.NODE_NAME.matches(name)) throw UsageError("a node name matches ${HubConfig.NODE_NAME.pattern}")
        val publicUrl = hub.config().publicUrl
        val script = "https://raw.githubusercontent.com/xoadev/limen/main/install.sh"
        if (publicUrl == null) {
            // No HTTP hub to call back: the line carries the key, and the machine prints what to trust here.
            val key = Keys.withoutComment(hub.publicKey)
            Sys.out("On $name, as root:\n")
            Sys.out("  curl -fsSL $script | sudo sh -s -- --hub-key '$key' --name $name\n")
            Sys.out("OpenWrt:\n")
            Sys.out("  wget -qO- $script | sh -s -- --hub-key '$key' --name $name\n")
            Sys.out("It ends printing a `limen trust` line to run here.\n")
            return
        }
        val ttlValue = Durations.parse(ttl) ?: throw UsageError("--ttl takes a duration like 30m or 2h")
        val issued = hub.invite(name, ttlValue)
        val join = JoinUrl(publicUrl, issued.code, Keys.fingerprint(hub.publicKey), issued.secret)
        Sys.out("On $name, as root:\n")
        Sys.out("  curl -fsSL $script | sudo sh -s -- --join '$join'\n")
        Sys.out("OpenWrt:\n")
        Sys.out("  wget -qO- $script | sh -s -- --join '$join'\n")
        Sys.out("Valid once, for $ttl.\n")
    }
}

private class TrustCommand : CoreCliktCommand("trust") {
    override fun help(context: Context) = "Add a machine to this hub by hand: its name, address and host key"

    val name by argument()
    val address by argument()
    val hostKey by argument(name = "host-key")
    val user by option("--user").default(HubConfig.READ_USER).help("root for OpenWrt")
    val port by option("--port").default("22")
    val home by option("--home").help("Hub directory")

    override fun run() {
        Hub.at(home).trust(name, address, hostKey, user, port.toIntOrNull() ?: throw UsageError("--port takes a number"))
        Sys.out("$name added; try it with `limen call $name hello`\n")
    }
}

private class ForgetCommand : CoreCliktCommand("forget") {
    override fun help(context: Context) = "Remove a machine from this hub (uninstall on the machine is separate)"

    val name by argument()
    val home by option("--home").help("Hub directory")

    override fun run() {
        if (!Hub.at(home).remove(name)) throw UsageError("no node named '$name'")
        Sys.out("$name removed from the hub\n")
    }
}

private class JoinCommand : CoreCliktCommand("join") {
    override fun help(context: Context) =
        "Join this machine to a hub (as root): with the line of `limen invite`, or with --hub-key and --name when the hub is not reachable over HTTP"

    val url by argument(help = "The join line of `limen invite`").optional()
    val hubKey by option("--hub-key").help("The hub's public key, when there is no join line")
    val name by option("--name").help("This machine's name on the hub, with --hub-key")
    val deployKey by option("--deploy-key").help("Public key for the deploy role")
    val from by option("--from").help("Addresses or CIDRs the keys may connect from (not OpenWrt)")
    val repo by option("--repo").help("Git repository this machine follows")
    val branch by option("--branch").default("main")
    val path by option("--path").help("This machine's folder in --repo (default: nodes/<its name on the hub>)")
    val address by option("--address").help("Where the hub reaches this machine (default: where the join request comes from)")
    val sshPort by option("--ssh-port").default("22")

    override fun run() {
        val port = sshPort.toIntOrNull() ?: throw UsageError("--ssh-port takes a number")
        val joiner = Joiner(Installer(dryRun = false))
        val code =
            try {
                val target = url
                val key = hubKey
                when {
                    target != null -> {
                        val join = JoinUrl.parse(target)
                        // The folder defaults to the name the invitation gives, known only once the hub answers.
                        val repoFor = repo?.let { url -> { invited: String -> RepoOptions(url, branch, path ?: "nodes/$invited") } }
                        joiner.join(join, deployKey, from, repoFor, address, port)
                    }

                    key != null -> {
                        val nodeName = name ?: throw UsageError("--hub-key needs --name")
                        joiner.withKey(key, nodeName, deployKey, from, repoOptions(nodeName), port)
                    }

                    else -> {
                        throw UsageError("give the join line of `limen invite`, or --hub-key and --name")
                    }
                }
            } catch (e: Installer.InstallException) {
                Sys.err("limen: join: ${e.message}\n")
                1
            }
        throw ExitWith(code)
    }

    private fun repoOptions(nodeName: String) = repo?.let { RepoOptions(it, branch, path ?: "nodes/$nodeName") }
}

private class CallCommand : CoreCliktCommand("call") {
    override fun help(context: Context) =
        "One request to a node over SSH, printed as JSON. Checks as check_<name>; deploy requests " +
            "(apply, action) stream their output and take the deploy role's --user and --identity"

    val node by argument()
    val request by argument()
    val args by option("--arg").multiple().help("key=value, repeatable")
    val home by option("--home").help("Hub directory with limen.toml")
    val user by option("--user").help("SSH user for deploy requests (default: limen-deploy)")
    val identity by option("--identity").help("SSH key for deploy requests (default: the hub's)")

    override fun run() {
        val hub = LiveHub(Hub.at(home))
        if (hub.config().node(node) == null) throw UsageError("no node named '$node'")
        val (name, body) =
            if (request.startsWith("check_")) {
                val check = request.removePrefix("check_")
                val spec = catalog(hub)?.checks?.firstOrNull { it.name == check }
                "check" to
                    buildJsonObject {
                        put("name", check)
                        put("args", parseArgs(args, spec?.params.orEmpty()))
                    }
            } else if (request == "action") {
                // `--arg name=<action>`; every other argument is the action's own.
                val action =
                    args.firstOrNull { it.startsWith("name=") }?.substringAfter('=') ?: throw UsageError("action needs --arg name=<action>")
                val spec = catalog(hub)?.actions?.firstOrNull { it.name == action }
                "action" to
                    buildJsonObject {
                        put("name", action)
                        put("args", parseArgs(args.filterNot { it.startsWith("name=") }, spec?.params.orEmpty()))
                    }
            } else {
                val def = Requests.find(request) ?: throw UsageError("unknown request '$request'")
                request to parseArgs(args, def.params)
            }
        if (Requests.find(name)!!.role == Role.DEPLOY) {
            val line = WireJson.encodeToString(NodeRequest.serializer(), NodeRequest(1, name, body)) + "\n"
            val r =
                hub.ssh.stream(node, user ?: "limen-deploy", identity, line, 6.hours) { fd, bytes ->
                    if (fd == 1) Sys.outBytes(bytes) else Sys.err(bytes.decodeToString())
                }
            if (r.exitCode == 255) Sys.err("limen: cannot reach $node\n")
            throw ExitWith(r.exitCode)
        }
        if (user != null || identity != null) throw UsageError("--user and --identity are only for deploy requests")
        val response = runBlocking { hub.call(node, name, body) }
        Sys.out(PrettyJson.encodeToString(NodeResponse.serializer(), response) + "\n")
        if (!response.ok) throw ExitWith(1)
    }

    /** The node's scripts, so their arguments are typed as their headers say: `--arg tag=20` stays a string. */
    private fun catalog(hub: LiveHub): Catalog? {
        val hello = runBlocking { hub.call(node, "hello", JsonObject(emptyMap())) }
        val catalog = (hello.data as? JsonObject)?.get("catalog") ?: return null
        return runCatching { LenientJson.decodeFromJsonElement(Catalog.serializer(), catalog) }.getOrNull()
    }
}

private class GateCommand : CoreCliktCommand("gate") {
    override fun help(context: Context) = "The SSH forced command on a node: one JSON request on stdin, the answer on stdout"

    val role by option("--role").choice("read", "deploy").required()
    val config by option("--config").default(NodeConfig.PATH).help("Node configuration")

    override fun run(): Unit = throw ExitWith(Gate.run(Role.parse(role)!!, config))
}

private class InstallCommand : CoreCliktCommand("install") {
    override fun help(context: Context) = "Set this node up: binary, users, authorized_keys, sudoers, /etc/limen (as root)"

    val readKey by option("--read-key").required().help("Public key of the hub (read role)")
    val deployKey by option("--deploy-key").help("Public key of CI or a person (deploy role); without it, no deploy role")
    val from by option("--from").help("Addresses or CIDRs the keys may connect from, e.g. 100.64.0.0/10")
    val repo by option("--repo").help("Git repository with this node's scripts, stacks and node.toml (https:// asks for a token)")
    val branch by option("--branch").default("main").help("Branch of --repo")
    val path by option("--path").help("This node's folder in --repo (default: nodes/<hostname>)")
    val dryRun by option("--dry-run").flag().help("Say what would change and change nothing")

    override fun run() {
        val repoOptions = repo?.let { RepoOptions(it, branch, path ?: "nodes/${Sys.hostname().lowercase()}") }
        try {
            throw ExitWith(Installer(dryRun).install(readKey, deployKey, from, repoOptions))
        } catch (e: Installer.InstallException) {
            Sys.err("limen: install: ${e.message}\n")
            throw ExitWith(1)
        }
    }
}

private class UninstallCommand : CoreCliktCommand("uninstall") {
    override fun help(context: Context) = "Undo install (as root)"

    val purge by option("--purge").flag().help("Also remove /etc/limen and /var/log/limen")
    val dryRun by option("--dry-run").flag().help("Say what would change and change nothing")

    override fun run() {
        try {
            throw ExitWith(Installer(dryRun).uninstall(purge))
        } catch (e: Installer.InstallException) {
            Sys.err("limen: uninstall: ${e.message}\n")
            throw ExitWith(1)
        }
    }
}

private class SyncCommand : CoreCliktCommand("sync") {
    override fun help(context: Context) = "Bring this node's copy of its repository to the remote branch"

    val config by option("--config").default(NodeConfig.PATH)

    override fun run(): Unit = throw ExitWith(if (Deploy.sync(Node.load(config))) 0 else 1)
}

private class ApplyCommand : CoreCliktCommand("apply") {
    override fun help(context: Context) = "Sync the repository, run the setup scripts in order and bring up the stacks, on this node"

    val from by option("--from").help("Start at the script with this number prefix")
    val noSync by option("--no-sync").flag().help("Use the checkout as it is")
    val dryRun by option("--dry-run").flag().help("List what would run")
    val config by option("--config").default(NodeConfig.PATH)

    override fun run() {
        val start = from
        if (start != null && !Regex("^[0-9]{1,4}$").matches(start)) throw UsageError("--from takes the number prefix, e.g. 20")
        Sys.chdirRoot()
        throw ExitWith(if (Deploy.apply(Node.load(config), start, dryRun, syncFirst = !noSync)) 0 else 1)
    }
}

private class TokenCommand : CoreCliktCommand("token") {
    override fun help(context: Context) = "Replace the token that reads this node's repository (as root)"

    override fun run() {
        try {
            throw ExitWith(Installer(dryRun = false).token())
        } catch (e: Installer.InstallException) {
            Sys.err("limen: token: ${e.message}\n")
            throw ExitWith(1)
        }
    }
}

private class ActionCommand : CoreCliktCommand("action") {
    override fun help(context: Context) = "Run one action script, on this node"

    val name by argument()
    val args by option("--arg").multiple().help("key=value, repeatable")
    val config by option("--config").default(NodeConfig.PATH)

    override fun run() {
        Sys.chdirRoot()
        val node = Node.load(config)
        val (_, spec) = Scripts.find(node, ScriptKind.ACTION, name)
        throw ExitWith(if (Deploy.action(node, name, parseArgs(args, spec.params))) 0 else 1)
    }
}

private class CheckCommand : CoreCliktCommand("check") {
    override fun help(context: Context) = "Run one check script, on this node; exits with its code"

    val name by argument()
    val args by option("--arg").multiple().help("key=value, repeatable")
    val config by option("--config").default(NodeConfig.PATH)

    override fun run() {
        Sys.chdirRoot()
        val node = Node.load(config)
        val answer =
            try {
                val (_, spec) = Scripts.find(node, ScriptKind.CHECK, name)
                val body = JsonObject(mapOf("name" to JsonPrimitive(name), "args" to parseArgs(args, spec.params)))
                Read.check(node, Args.validate(Requests.CHECK.params, body))
            } catch (e: LimenException) {
                // Not found, a timeout, bad arguments: no answer from the check is Nagios' UNKNOWN, not a warning.
                Sys.err("limen: check: ${e.code.wire}: ${e.message}\n")
                throw ExitWith(3)
            }
        Sys.out(PrettyJson.encodeToString(JsonElement.serializer(), answer.data) + "\n")
        val status = ((answer.data as JsonObject)["status"] as? JsonPrimitive)?.content
        throw ExitWith(listOf("ok", "warn", "fail").indexOf(status).takeIf { it >= 0 } ?: 3)
    }
}

private class LintCommand : CoreCliktCommand("lint") {
    override fun help(context: Context) = "Check script names, headers and permissions without running anything"

    val config by option("--config").default(NodeConfig.PATH)

    override fun run(): Unit = throw ExitWith(Lint.run(Node.load(config)))
}

private class VersionCommand : CoreCliktCommand("version") {
    override fun help(context: Context) = "Print the version"

    override fun run() = Sys.out(versionLine() + "\n")
}

/** `limen 0.3.1 · build 42 · 2026-09-26T08:00:00Z`, or `limen dev · 2026-09-26` for a local build. */
fun versionLine(): String =
    listOfNotNull("limen $LIMEN_VERSION", LIMEN_BUILD_NUMBER.takeIf { it.isNotEmpty() }?.let { "build $it" }, LIMEN_BUILD_DATE)
        .joinToString(" · ")
