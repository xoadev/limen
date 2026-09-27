package limen.core.toml

/**
 * Typed access to a [TomlTable] that names the key in every error (`files.allow: expected an array of strings`)
 * and rejects keys nobody reads, so a typo in a configuration file fails loudly instead of being ignored.
 */
class TomlReader(
    private val table: TomlTable,
    private val where: String = "",
) {
    private val read = mutableSetOf<String>()

    fun string(key: String): String? = get(key)?.let { (it as? TomlString)?.value ?: fail(key, "expected a string") }

    fun long(key: String): Long? = get(key)?.let { (it as? TomlInt)?.value ?: fail(key, "expected an integer") }

    fun int(key: String): Int? = long(key)?.let { if (it in Int.MIN_VALUE..Int.MAX_VALUE) it.toInt() else fail(key, "out of range") }

    fun bool(key: String): Boolean? = get(key)?.let { (it as? TomlBool)?.value ?: fail(key, "expected true or false") }

    fun strings(key: String): List<String>? =
        get(key)?.let { value ->
            val array = value as? TomlArray ?: fail(key, "expected an array of strings")
            array.items.map { (it as? TomlString)?.value ?: fail(key, "expected an array of strings") }
        }

    fun longs(key: String): List<Long>? =
        get(key)?.let { value ->
            val array = value as? TomlArray ?: fail(key, "expected an array of integers")
            array.items.map { (it as? TomlInt)?.value ?: fail(key, "expected an array of integers") }
        }

    fun raw(key: String): TomlValue? = get(key)

    fun table(key: String): TomlReader? = get(key)?.let { TomlReader(it as? TomlTable ?: fail(key, "expected a table"), path(key)) }

    /** Every entry of this table as a sub-table, for tables whose keys the operator names (`[nodes.<name>]`). */
    fun tables(): Map<String, TomlReader> = table.entries.keys.associateWith { key -> table(key)!! }

    /** Fails on any key that no accessor asked for. Call after reading everything. */
    fun rejectUnknown() {
        val unknown = table.entries.keys - read
        if (unknown.isNotEmpty()) fail(unknown.first(), "unknown key")
    }

    fun fail(
        key: String,
        message: String,
    ): Nothing = throw TomlException("${path(key)}: $message")

    private fun get(key: String): TomlValue? {
        read += key
        return table.entries[key]
    }

    private fun path(key: String) = if (where.isEmpty()) key else "$where.$key"
}
