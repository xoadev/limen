package limen.core.toml

/**
 * The subset of TOML that limen's files use: tables (`[a.b]`), dotted and quoted keys, basic and literal strings,
 * integers, booleans and arrays, which may span lines and end in a comma. No inline tables, arrays of tables,
 * floats, dates or multi-line strings: a file that uses them is rejected with its line, never half-read.
 *
 * Own parser rather than a library: the configuration and the script headers map TOML tables with names chosen by
 * the operator (`[nodes.nas]`, `[args.threshold]`) onto typed values with errors that name the key, and that is
 * simpler to say over a tree than through a deserializer.
 */
sealed interface TomlValue

data class TomlString(
    val value: String,
) : TomlValue

data class TomlInt(
    val value: Long,
) : TomlValue

data class TomlBool(
    val value: Boolean,
) : TomlValue

data class TomlArray(
    val items: List<TomlValue>,
) : TomlValue

class TomlTable(
    val entries: MutableMap<String, TomlValue> = linkedMapOf(),
) : TomlValue {
    /** Tables that a `[header]` opened. A table only created on the way to a dotted key can still get its header. */
    internal var declared = false
}

class TomlException(
    message: String,
) : Exception(message)

object Toml {
    fun parse(text: String): TomlTable = Parser(text).parse()
}

private class Parser(
    private val text: String,
) {
    private var pos = 0
    private var line = 1

    fun parse(): TomlTable {
        val root = TomlTable()
        var current = root
        while (true) {
            skipBlank(newlines = true)
            if (eof()) break
            if (peek() == '[') {
                pos++
                if (peekOr() == '[') fail("arrays of tables are not supported")
                skipBlank(newlines = false)
                val path = keyPath()
                skipBlank(newlines = false)
                expect(']')
                current = tableAt(root, path, header = true)
            } else {
                val path = keyPath()
                skipBlank(newlines = false)
                expect('=')
                skipBlank(newlines = false)
                val value = value()
                val parent = tableAt(current, path.dropLast(1), header = false)
                val key = path.last()
                if (key in parent.entries) fail("duplicate key '${path.joinToString(".")}'")
                parent.entries[key] = value
            }
            endOfLine()
        }
        return root
    }

    private fun tableAt(
        from: TomlTable,
        path: List<String>,
        header: Boolean,
    ): TomlTable {
        var table = from
        for ((i, key) in path.withIndex()) {
            val next =
                when (val existing = table.entries[key]) {
                    null -> TomlTable().also { table.entries[key] = it }
                    is TomlTable -> existing
                    else -> fail("'${path.take(i + 1).joinToString(".")}' is already a value, not a table")
                }
            table = next
        }
        if (header) {
            if (table.declared) fail("table [${path.joinToString(".")}] is defined twice")
            table.declared = true
        }
        return table
    }

    private fun keyPath(): List<String> {
        val keys = mutableListOf(key())
        while (true) {
            skipBlank(newlines = false)
            if (peekOr() != '.') return keys
            pos++
            skipBlank(newlines = false)
            keys += key()
        }
    }

    private fun key(): String =
        when (peekOr()) {
            '"' -> {
                basicString()
            }

            '\'' -> {
                literalString()
            }

            else -> {
                val start = pos
                while (!eof() && (peek().isLetterOrDigit() || peek() == '_' || peek() == '-')) pos++
                if (start == pos) fail("expected a key")
                text.substring(start, pos)
            }
        }

    private fun value(): TomlValue =
        when (val c = peekOr()) {
            '"' -> TomlString(basicString())
            '\'' -> TomlString(literalString())
            '[' -> array()
            't', 'f' -> bool()
            null -> fail("expected a value")
            else -> if (c.isDigit() || c == '+' || c == '-') integer() else fail("unsupported value starting with '$c'")
        }

    private fun array(): TomlArray {
        expect('[')
        val items = mutableListOf<TomlValue>()
        while (true) {
            skipBlank(newlines = true)
            if (peekOr() == ']') {
                pos++
                return TomlArray(items)
            }
            items += value()
            skipBlank(newlines = true)
            when (peekOr()) {
                ',' -> {
                    pos++
                }

                ']' -> {
                    pos++
                    return TomlArray(items)
                }

                else -> {
                    fail("expected ',' or ']' in an array")
                }
            }
        }
    }

    private fun bool(): TomlBool =
        when {
            text.startsWith("true", pos) -> TomlBool(true).also { pos += 4 }
            text.startsWith("false", pos) -> TomlBool(false).also { pos += 5 }
            else -> fail("expected true or false")
        }

    private fun integer(): TomlInt {
        val start = pos
        if (peek() == '+' || peek() == '-') pos++
        while (!eof() && (peek().isDigit() || peek() == '_')) pos++
        val digits = text.substring(start, pos).replace("_", "")
        if (!eof() && (peek() == '.' || peek() == 'e' || peek() == 'E')) fail("floats are not supported")
        return TomlInt(digits.toLongOrNull() ?: fail("'$digits' is not an integer"))
    }

    private fun basicString(): String {
        expect('"')
        if (text.startsWith("\"\"", pos)) fail("multi-line strings are not supported")
        val out = StringBuilder()
        while (true) {
            if (eof() || peek() == '\n') fail("unterminated string")
            val c = text[pos++]
            when (c) {
                '"' -> return out.toString()
                '\\' -> out.append(escape())
                else -> out.append(c)
            }
        }
    }

    private fun escape(): String {
        if (eof()) fail("unterminated escape")
        return when (val c = text[pos++]) {
            '"' -> "\""
            '\\' -> "\\"
            'n' -> "\n"
            't' -> "\t"
            'r' -> "\r"
            'b' -> "\b"
            'f' -> "\u000C"
            'u' -> unicode(4)
            'U' -> unicode(8)
            else -> fail("unknown escape '\\$c'")
        }
    }

    private fun unicode(length: Int): String {
        if (pos + length > text.length) fail("truncated unicode escape")
        val code = text.substring(pos, pos + length).toIntOrNull(16) ?: fail("bad unicode escape")
        pos += length
        if (code > 0x10FFFF) fail("bad unicode escape")
        return if (code < 0x10000) {
            code.toChar().toString()
        } else {
            val v = code - 0x10000
            charArrayOf((0xD800 + (v shr 10)).toChar(), (0xDC00 + (v and 0x3FF)).toChar()).concatToString()
        }
    }

    private fun literalString(): String {
        expect('\'')
        if (text.startsWith("''", pos)) fail("multi-line strings are not supported")
        val start = pos
        while (true) {
            if (eof() || peek() == '\n') fail("unterminated string")
            if (text[pos++] == '\'') return text.substring(start, pos - 1)
        }
    }

    /** Spaces, tabs and comments; with [newlines], also line breaks, counting them. */
    private fun skipBlank(newlines: Boolean) {
        while (!eof()) {
            when (peek()) {
                ' ', '\t', '\r' -> {
                    pos++
                }

                '#' -> {
                    while (!eof() && peek() != '\n') pos++
                }

                '\n' -> {
                    if (newlines) {
                        pos++
                        line++
                    } else {
                        return
                    }
                }

                else -> {
                    return
                }
            }
        }
    }

    private fun endOfLine() {
        skipBlank(newlines = false)
        if (eof()) return
        if (peek() != '\n') fail("unexpected '${peek()}' after a value")
        pos++
        line++
    }

    private fun expect(c: Char) {
        if (peekOr() != c) fail("expected '$c'")
        pos++
    }

    private fun eof() = pos >= text.length

    private fun peek() = text[pos]

    private fun peekOr(): Char? = if (eof()) null else text[pos]

    private fun fail(message: String): Nothing = throw TomlException("line $line: $message")
}
