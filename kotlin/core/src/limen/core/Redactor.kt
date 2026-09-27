package limen.core

/**
 * Masks secrets in text going out of the node: files, logs, check output, process arguments (spec §7.1). A
 * pattern with a group named `secret` replaces only that group, so `password=hunter2` stays readable as
 * `password=[redacted]`; without it the whole match goes.
 *
 * Best-effort by nature: the protection is not allowing files that hold secrets. This only catches the usual shapes.
 */
class Redactor(
    extra: List<String> = emptyList(),
) {
    private val patterns: List<Regex> = (BUILT_IN + extra).map { Regex(it) }

    fun redact(text: String): String {
        var out = text
        for (regex in patterns) {
            out =
                regex.replace(out) { match ->
                    val secret = runCatching { match.groups["secret"] }.getOrNull()
                    if (secret == null) {
                        MASK
                    } else {
                        val start = secret.range.first - match.range.first
                        val end = secret.range.last + 1 - match.range.first
                        match.value.substring(0, start) + MASK + match.value.substring(end)
                    }
                }
        }
        return out
    }

    companion object {
        const val MASK = "[redacted]"

        private const val SECRET_NAME =
            "(?i)(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret)"

        val BUILT_IN =
            listOf(
                // key = value, key: value, --key=value, "key": "value", for the usual names of secrets. A quoted value
                // is masked to its closing quote (or the end of the line), spaces included; a bare one to the next
                // space or separator.
                "$SECRET_NAME[\"']?\\s*[:=]\\s*\"(?<secret>[^\"\\n]*)",
                "$SECRET_NAME[\"']?\\s*[:=]\\s*'(?<secret>[^'\\n]*)",
                "$SECRET_NAME[\"']?\\s*[:=]\\s*(?<secret>[^\\s\"',;&]+)",
                // --password value: a flag and its value, apart.
                "(?i)(?<![\\w-])--?${SECRET_NAME.removePrefix("(?i)")}\\s+(?<secret>[^\\s\"'-][^\\s\"']*)",
                """(?i)authorization:\s*(?:bearer|basic|token)\s+(?<secret>\S+)""",
                // Credentials inside a URL: scheme://user:password@host.
                """[a-zA-Z][a-zA-Z0-9+.-]*://[^/\s:@]+:(?<secret>[^@\s/]+)@""",
                "-----BEGIN [A-Z ]*PRIVATE KEY-----[\\s\\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            )
    }
}
