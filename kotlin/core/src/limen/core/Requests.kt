package limen.core

import kotlinx.serialization.json.JsonPrimitive

enum class Role(
    val wire: String,
) {
    READ("read"),
    DEPLOY("deploy"),
    ;

    companion object {
        fun parse(text: String): Role? = entries.firstOrNull { it.wire == text }
    }
}

/**
 * A request the gate answers (spec §4, §5). [tool] says whether the hub exposes it as an MCP tool as it is; `hello`
 * and `check` are requests the hub turns into `nodes` and `check_<name>` instead.
 */
data class RequestDef(
    val name: String,
    val role: Role,
    val description: String,
    val params: List<Param> = emptyList(),
    val tool: Boolean = true,
)

object Requests {
    const val UNIT = "^[A-Za-z0-9@._:\\\\-]{1,256}$"
    private const val UNIT_GLOB = "^[A-Za-z0-9@._:*?\\[\\]\\\\-]{1,256}$"
    const val CONTAINER = "^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$"
    private const val ABSOLUTE_PATH = "^/[^\\u0000-\\u001f]{0,4095}$"
    private const val TIME = "^(\\d{1,6}[smhd]|\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}(:\\d{2})?Z)$"
    const val SCRIPT_NAME = "^[a-z0-9][a-z0-9_-]{0,47}$"

    private val time = "Relative (`30m`, `2h`, `1d`: that long ago) or absolute UTC (`2026-09-26T08:00:00Z`)"

    val HELLO =
        RequestDef(
            "hello",
            Role.READ,
            "Node facts, limen version and the catalog of scripts.",
            tool = false,
        )

    val STATUS =
        RequestDef(
            "status",
            Role.READ,
            "Overview of a node: uptime, load, memory, disk usage per mount, failed services (systemd, or procd on OpenWrt), " +
                "containers that are not running or unhealthy, and whether a reboot is pending. Start here.",
        )

    val SERVICES =
        RequestDef(
            "services",
            Role.READ,
            "Services with their state: systemd units (load, active and sub state), or procd services on OpenWrt. `type` is systemd's.",
            listOf(
                Param(
                    "state",
                    ParamType.ENUM,
                    "Filter by state",
                    default = JsonPrimitive("all"),
                    values = listOf("all", "running", "failed", "active", "inactive", "exited"),
                ),
                Param(
                    "type",
                    ParamType.ENUM,
                    "Unit type",
                    default = JsonPrimitive("service"),
                    values = listOf("service", "timer", "socket", "mount", "path", "target", "all"),
                ),
                Param("pattern", ParamType.STRING, "Glob on the unit name, e.g. `docker*`", pattern = UNIT_GLOB),
            ),
        )

    val SERVICE =
        RequestDef(
            "service",
            Role.READ,
            "One service with its last log lines: a systemd unit's state, result, restarts, main PID, memory, unit file and " +
                "enablement, or a procd service's instances on OpenWrt.",
            listOf(
                Param(
                    "name",
                    ParamType.STRING,
                    "Unit or service name; `.service` is assumed without a suffix",
                    required = true,
                    pattern = UNIT,
                ),
                Param("lines", ParamType.INT, "Journal lines to include", default = JsonPrimitive(20), min = 0, max = 200),
            ),
        )

    val CONTAINERS =
        RequestDef(
            "containers",
            Role.READ,
            "Docker containers: name, image, state, health, restarts, start time and compose project.",
            listOf(Param("all", ParamType.BOOL, "Include stopped containers", default = JsonPrimitive(true))),
        )

    val CONTAINER_DETAIL =
        RequestDef(
            "container",
            Role.READ,
            "One Docker container: image and digest, state, health checks, mounts, ports, networks, labels and " +
                "restart policy. Environment variables are listed by name only.",
            listOf(Param("name", ParamType.STRING, "Container name or ID", required = true, pattern = CONTAINER)),
        )

    val LOGS =
        RequestDef(
            "logs",
            Role.READ,
            "Log lines from a service (systemd's journal, or logread on OpenWrt), the whole log, a Docker container, or an " +
                "allowed file. Always a bounded window: the last `lines` lines, optionally within `since`/`until` and matching `grep`.",
            listOf(
                Param("source", ParamType.ENUM, "Where to read", required = true, values = listOf("unit", "journal", "container", "file")),
                Param(
                    "name",
                    ParamType.STRING,
                    "Unit name, container name, or absolute file path. Not used with `journal`",
                    pattern = "^[^\\u0000-\\u001f]{1,4096}$",
                ),
                Param("since", ParamType.STRING, "Start of the window. $time. Not for files", pattern = TIME),
                Param("until", ParamType.STRING, "End of the window. $time. Not for files", pattern = TIME),
                // No maximum here: the node cuts at its logs.max_lines and says so, whatever the operator set it to.
                Param("lines", ParamType.INT, "How many lines, newest last", default = JsonPrimitive(200), min = 1),
                Param(
                    "grep",
                    ParamType.STRING,
                    "Only lines containing this text, case-insensitive",
                    pattern = "^[^\\u0000-\\u001f]{1,256}$",
                ),
                Param(
                    "priority",
                    ParamType.ENUM,
                    "Journal only: this priority and more severe",
                    values = listOf("emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"),
                ),
            ),
        )

    val READ_FILE =
        RequestDef(
            "read_file",
            Role.READ,
            "Contents of a file the node allows, by line range.",
            listOf(
                Param("path", ParamType.STRING, "Absolute path", required = true, pattern = ABSOLUTE_PATH),
                Param("from", ParamType.INT, "First line, from 1", default = JsonPrimitive(1), min = 1),
                Param("lines", ParamType.INT, "How many lines", default = JsonPrimitive(500), min = 1, max = 20000),
            ),
        )

    val LIST_DIR =
        RequestDef(
            "list_dir",
            Role.READ,
            "Entries of a directory the node allows, or that leads to allowed files: name, type, size, owner, mode, " +
                "modification time.",
            listOf(Param("path", ParamType.STRING, "Absolute path", required = true, pattern = ABSOLUTE_PATH)),
        )

    val PROCESSES =
        RequestDef(
            "processes",
            Role.READ,
            "Top processes by CPU or memory.",
            listOf(
                Param("sort", ParamType.ENUM, "Order", default = JsonPrimitive("cpu"), values = listOf("cpu", "memory")),
                Param("limit", ParamType.INT, "How many", default = JsonPrimitive(20), min = 1, max = 200),
            ),
        )

    val PORTS = RequestDef("ports", Role.READ, "Listening TCP and UDP sockets and the processes behind them.")

    val HISTORY =
        RequestDef(
            "history",
            Role.READ,
            "The node's audit log: every request limen answered, with role, arguments, client and result.",
            listOf(Param("lines", ParamType.INT, "How many entries, newest last", default = JsonPrimitive(50), min = 1, max = 1000)),
        )

    val STATE =
        RequestDef(
            "state",
            Role.READ,
            "What is deployed against what should be: the repository commit on the node and on the remote, and every " +
                "service node.toml expects (compose stacks, systemd units, procd services) with whether it runs.",
        )

    val CHECK =
        RequestDef(
            "check",
            Role.READ,
            "Runs a check script.",
            listOf(
                Param("name", ParamType.STRING, "Check name", required = true, pattern = SCRIPT_NAME),
                Param("args", ParamType.OBJECT, "The check's arguments", default = null),
            ),
            tool = false,
        )

    val SYNC =
        RequestDef(
            "sync",
            Role.DEPLOY,
            "Brings the node's copy of the repository to the remote branch.",
            tool = false,
        )

    val APPLY =
        RequestDef(
            "apply",
            Role.DEPLOY,
            "Runs every setup script in order.",
            listOf(
                Param("from", ParamType.STRING, "Start at the script with this prefix", pattern = "^[0-9]{1,4}$"),
                Param("dry_run", ParamType.BOOL, "List what would run", default = JsonPrimitive(false)),
                Param("sync", ParamType.BOOL, "Sync the repository first", default = JsonPrimitive(true)),
            ),
            tool = false,
        )

    val ACTION =
        RequestDef(
            "action",
            Role.DEPLOY,
            "Runs one action script.",
            listOf(
                Param("name", ParamType.STRING, "Action name", required = true, pattern = SCRIPT_NAME),
                Param("args", ParamType.OBJECT, "The action's arguments"),
            ),
            tool = false,
        )

    val all =
        listOf(
            HELLO,
            STATUS,
            SERVICES,
            SERVICE,
            CONTAINERS,
            CONTAINER_DETAIL,
            LOGS,
            READ_FILE,
            LIST_DIR,
            PROCESSES,
            PORTS,
            HISTORY,
            STATE,
            CHECK,
            SYNC,
            APPLY,
            ACTION,
        )

    fun find(name: String): RequestDef? = all.firstOrNull { it.name == name }
}
