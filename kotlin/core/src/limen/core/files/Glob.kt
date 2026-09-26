package limen.core.files

/**
 * A path pattern of `files.allow` and `files.deny` (spec §7.1). Absolute; `**` is any number of segments, zero
 * included, so a trailing `**` also matches the directory itself; `*` is any run of characters within a segment and
 * `?` one character. Nothing else is special.
 */
class Glob(
    val pattern: String,
) {
    private val segments: List<String>

    init {
        require(pattern.startsWith("/") || pattern.startsWith("**")) { "'$pattern' is not an absolute pattern" }
        segments = split(pattern)
    }

    fun matches(path: String): Boolean = match(segments, 0, split(path), 0, prefixOnly = false)

    /** Whether something at or below directory [dir] could match: what lets `list_dir` walk towards allowed files. */
    fun mayMatchBelow(dir: String): Boolean = match(segments, 0, split(dir), 0, prefixOnly = true)

    override fun toString() = pattern

    private fun match(
        pat: List<String>,
        pi: Int,
        path: List<String>,
        si: Int,
        prefixOnly: Boolean,
    ): Boolean {
        if (si == path.size && prefixOnly) return true
        if (pi == pat.size) return si == path.size
        if (pat[pi] == "**") {
            for (skip in si..path.size) {
                if (match(pat, pi + 1, path, skip, prefixOnly)) return true
            }
            return false
        }
        if (si == path.size) return false
        return segment(pat[pi], path[si]) && match(pat, pi + 1, path, si + 1, prefixOnly)
    }

    companion object {
        fun split(path: String): List<String> = path.split('/').filter { it.isNotEmpty() }

        /** `*` and `?` within one segment, by backtracking over the last `*`. */
        fun segment(
            pattern: String,
            text: String,
        ): Boolean {
            var p = 0
            var t = 0
            var star = -1
            var mark = 0
            while (t < text.length) {
                when {
                    p < pattern.length && (pattern[p] == '?' || pattern[p] == text[t]) -> {
                        p++
                        t++
                    }

                    p < pattern.length && pattern[p] == '*' -> {
                        star = p++
                        mark = t
                    }

                    star >= 0 -> {
                        p = star + 1
                        t = ++mark
                    }

                    else -> {
                        return false
                    }
                }
            }
            while (p < pattern.length && pattern[p] == '*') p++
            return p == pattern.length
        }
    }
}
