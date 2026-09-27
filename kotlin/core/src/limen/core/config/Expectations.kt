package limen.core.config

import limen.core.Requests
import limen.core.toml.Toml
import limen.core.toml.TomlReader

/**
 * `node.toml` in the node's folder of the repository (spec §6.1): what must be running. The scripts are how the
 * node gets there; this is what `state` compares against.
 */
data class Expectations(
    /** Docker Compose stacks: `stacks/<name>/compose.yaml`, brought up by `apply`. */
    val compose: List<String> = emptyList(),
    /** systemd units that must be active. */
    val units: List<String> = emptyList(),
    /** procd services (OpenWrt) that must have a running instance. */
    val procd: List<String> = emptyList(),
) {
    companion object {
        private val STACK = Regex("^[a-z0-9][a-z0-9_-]{0,62}$")
        private val UNIT = Regex(Requests.UNIT)
        private val SERVICE = Regex("^[A-Za-z0-9._-]{1,64}$")

        fun parse(text: String): Expectations {
            val root = TomlReader(Toml.parse(text))
            val expect = root.table("expect")
            val result =
                Expectations(
                    compose = expect?.strings("compose")?.also { check(expect, "compose", it, STACK) }.orEmpty(),
                    units = expect?.strings("units")?.also { check(expect, "units", it, UNIT) }.orEmpty(),
                    procd = expect?.strings("procd")?.also { check(expect, "procd", it, SERVICE) }.orEmpty(),
                )
            expect?.rejectUnknown()
            root.rejectUnknown()
            return result
        }

        private fun check(
            t: TomlReader,
            key: String,
            names: List<String>,
            shape: Regex,
        ) = names.forEach { if (!shape.matches(it)) t.fail(key, "'$it' is not a valid name") }
    }
}
