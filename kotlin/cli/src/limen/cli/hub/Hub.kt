package limen.cli.hub

import limen.cli.os.Fs
import limen.cli.os.Sys
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.config.HubConfig
import limen.core.toml.TomlException

/** The hub's configuration and its SSH client, from `$LIMEN_HOME/limen.toml` (spec §7.2). */
class Hub(
    val home: String,
    val config: HubConfig,
) {
    val ssh: SshClient by lazy { SshClient(config, home) }
    val client: NodeClient get() = ssh

    companion object {
        fun home(option: String?): String =
            option ?: Sys.env("LIMEN_HOME")?.takeIf { it.isNotBlank() } ?: ((Sys.env("HOME") ?: "/root") + "/.limen")

        fun load(option: String?): Hub {
            val home = home(option).trimEnd('/')
            val path = "$home/limen.toml"
            val text =
                Fs.realPath(path)?.let { Fs.readText(it) }
                    ?: throw LimenException(ErrorCode.UNAVAILABLE, "no $path; it lists the nodes and the SSH key (see the README)")
            val config =
                try {
                    HubConfig.parse(text)
                } catch (e: TomlException) {
                    throw LimenException(ErrorCode.BAD_REQUEST, "$path: ${e.message}")
                }
            if (config.nodes.isEmpty()) throw LimenException(ErrorCode.BAD_REQUEST, "$path lists no [nodes.<name>]")
            return Hub(home, config)
        }
    }
}
