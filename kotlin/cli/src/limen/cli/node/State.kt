package limen.cli.node

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import kotlinx.serialization.json.putJsonObject
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.core.ErrorCode
import limen.core.LenientJson
import limen.core.LimenException
import limen.core.config.Expectations
import limen.core.system.Parsers
import limen.core.toml.TomlException
import kotlin.time.Duration.Companion.minutes

/** One expected service and what the node says about it. */
class ServiceState(
    val kind: String,
    val name: String,
    val running: Boolean,
    val detail: JsonObject,
)

/**
 * `state` (spec §6.1): the repository commit on the node against the remote, and every service `node.toml` expects
 * against what runs. What `apply` checks at the end, and what an agent asks first after a deploy.
 */
object State {
    fun answer(node: Node): Answer {
        val repo = node.config.repo
        val problems = mutableListOf<String>()
        val repoJson =
            if (repo == null) {
                JsonNull
            } else {
                val deployed = Repo.deployed(repo)
                val remote = runCatching { Repo.remote(repo) }
                buildJsonObject {
                    put("url", repo.displayUrl)
                    put("branch", repo.branch)
                    put("path", repo.path)
                    put(
                        "deployed",
                        deployed?.let {
                            buildJsonObject {
                                put("commit", it.hash)
                                put("date", it.date)
                                put("subject", it.subject)
                            }
                        } ?: JsonNull,
                    )
                    remote.onSuccess { put("remote", it) }.onFailure { put("remote_error", it.message?.let(node.redactor::redact)) }
                    remote.getOrNull()?.let { put("up_to_date", deployed?.hash == it) }
                    // git's own words, which can hold a URL with its credentials.
                    put(
                        "last_sync",
                        Repo.lastSync(repo)?.let { LenientJson.parseToJsonElement(node.redactor.redact(it.toString())) } ?: JsonNull,
                    )
                }.also {
                    if (deployed == null) problems += "the repository is not checked out: run sync or apply"
                    if (remote.isSuccess && deployed != null &&
                        deployed.hash != remote.getOrNull()
                    ) {
                        problems += "the node is behind ${repo.branch}"
                    }
                }
            }
        val services =
            try {
                services(node)
            } catch (e: LimenException) {
                problems += e.message ?: "cannot read node.toml"
                emptyList()
            }
        services.filter { !it.running }.forEach { problems += "${it.kind} ${it.name} is not running" }
        return Answer(
            buildJsonObject {
                put("repo", repoJson)
                putJsonArray("services") {
                    services.forEach { s ->
                        add(
                            buildJsonObject {
                                put("kind", s.kind)
                                put("name", s.name)
                                put("running", s.running)
                                s.detail.forEach { (k, v) -> put(k, v) }
                            },
                        )
                    }
                }
                put("problems", JsonArray(problems.map(::JsonPrimitive)))
            },
        )
    }

    fun expectations(node: Node): Expectations {
        val path = node.config.expectations ?: return Expectations()
        val text = Fs.readText(path) ?: return Expectations()
        return try {
            Expectations.parse(text)
        } catch (e: TomlException) {
            throw LimenException(ErrorCode.INTERNAL, "$path: ${e.message}")
        }
    }

    fun services(node: Node): List<ServiceState> {
        val expected = expectations(node)
        return expected.compose.map { compose(node, it) } + expected.units.map { unit(node, it) } + expected.procd.map { procd(node, it) }
    }

    /** `stacks/<name>/compose.yaml` of the node's folder: the file limen brings up and then asks about. */
    fun composeFile(
        node: Node,
        name: String,
    ): String {
        val dir = node.config.stacks ?: throw LimenException(ErrorCode.UNAVAILABLE, "stacks need [repo] in limen.toml")
        return listOf("compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml")
            .map { "$dir/$name/$it" }
            .firstOrNull { Fs.exists(it) }
            ?: throw LimenException(ErrorCode.NOT_FOUND, "no compose file in $dir/$name")
    }

    private fun compose(
        node: Node,
        name: String,
    ): ServiceState {
        fun missing(reason: String) = ServiceState("compose", name, false, buildJsonObject { put("error", reason) })
        val file = runCatching { composeFile(node, name) }.getOrElse { return missing(it.message ?: "no compose file") }
        if (Proc.which("docker") == null) return missing("docker is not installed")
        val declared =
            node
                .exec("docker", "compose", "-p", name, "-f", file, "config", "--services", timeout = 1.minutes)
                .takeIf { it.exitCode == 0 }
                ?.out
                ?.lines()
                ?.map { it.trim() }
                ?.filter { it.isNotEmpty() }
                ?: return missing("docker compose config failed")
        val ps = node.exec("docker", "compose", "-p", name, "-f", file, "ps", "--all", "--format", "json", timeout = 1.minutes)
        if (ps.exitCode != 0) return missing("docker compose ps: ${ps.err.trim().lines().lastOrNull()}")
        // Older Compose prints one JSON array, newer one object per line.
        val containers =
            ps.out.trim().let { out ->
                if (out.startsWith("[")) {
                    runCatching { LenientJson.parseToJsonElement(out) as JsonArray }.getOrNull().orEmpty().map { it.jsonObject }
                } else {
                    out.lines().filter { it.isNotBlank() }.mapNotNull {
                        runCatching {
                            LenientJson
                                .parseToJsonElement(
                                    it,
                                ).jsonObject
                        }.getOrNull()
                    }
                }
            }

        fun field(
            o: JsonObject,
            key: String,
        ) = (o[key] as? JsonPrimitive)?.contentOrNull
        val byService = containers.groupBy { field(it, "Service") }
        val missingServices = declared.filter { byService[it].isNullOrEmpty() }
        val finished = { c: JsonObject -> field(c, "State") == "exited" && field(c, "ExitCode") == "0" }
        val notRunning = containers.filter { (field(it, "State") != "running" && !finished(it)) || field(it, "Health") == "unhealthy" }
        return ServiceState(
            "compose",
            name,
            missingServices.isEmpty() && notRunning.isEmpty(),
            buildJsonObject {
                putJsonArray("containers") {
                    containers.forEach { c ->
                        add(
                            buildJsonObject {
                                put("service", field(c, "Service"))
                                put("state", field(c, "State"))
                                put("health", field(c, "Health")?.ifEmpty { null })
                                put("image", field(c, "Image"))
                            },
                        )
                    }
                }
                if (missingServices.isNotEmpty()) put("missing", JsonArray(missingServices.map(::JsonPrimitive)))
            },
        )
    }

    private fun unit(
        node: Node,
        name: String,
    ): ServiceState {
        if (Platform.init !=
            Init.SYSTEMD
        ) {
            return ServiceState("unit", name, false, buildJsonObject { put("error", "no systemd on this node") })
        }
        val props =
            Parsers.keyValues(
                node.execOk("systemctl", "show", "--no-pager", "--property=LoadState,ActiveState,SubState", "--", name),
            )
        return ServiceState(
            "unit",
            name,
            props["ActiveState"] == "active",
            buildJsonObject {
                put("load", props["LoadState"])
                put("active", props["ActiveState"])
                put("sub", props["SubState"])
            },
        )
    }

    private fun procd(
        node: Node,
        name: String,
    ): ServiceState {
        if (Platform.init !=
            Init.PROCD
        ) {
            return ServiceState("procd", name, false, buildJsonObject { put("error", "no procd on this node") })
        }
        val s = runCatching { Platform.procdServices(node, name)[name] }.getOrNull()
        return ServiceState(
            "procd",
            name,
            s != null && s.state != Platform.ProcdState.FAILED,
            buildJsonObject {
                put("active", s?.state?.active ?: "missing")
                putJsonObject("instances") { s?.instances?.forEach { (k, v) -> put(k, (v as? JsonObject)?.get("running") ?: JsonNull) } }
            },
        )
    }
}
