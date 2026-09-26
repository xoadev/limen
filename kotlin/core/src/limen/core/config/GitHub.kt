package limen.core.config

/**
 * The link `install` prints to create the repository token (spec §10): GitHub's template URL for fine-grained
 * tokens, filled with a name, the owner, no expiry and read access to contents. The repository itself can't be
 * chosen by URL; the form asks for it.
 */
object GitHub {
    fun tokenUrl(
        owner: String,
        repo: String,
        host: String,
    ): String {
        val params =
            listOf(
                "name" to "limen-$host".take(40),
                "description" to "limen on $host: read $owner/$repo",
                "target_name" to owner,
                "expires_in" to "none",
                "contents" to "read",
            )
        return "https://github.com/settings/personal-access-tokens/new?" + params.joinToString("&") { (k, v) -> "$k=${encode(v)}" }
    }

    private fun encode(s: String): String =
        buildString {
            for (b in s.encodeToByteArray()) {
                val c = b.toInt() and 0xff
                if ((c < 128 && c.toChar().isLetterOrDigit()) || c.toChar() in "-._~") {
                    append(c.toChar())
                } else {
                    append('%').append(c.toString(16).uppercase().padStart(2, '0'))
                }
            }
        }
}
