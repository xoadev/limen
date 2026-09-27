package limen.cli

import io.ktor.utils.io.charsets.Charsets
import io.ktor.utils.io.charsets.encode
import kotlinx.io.readByteArray
import kotlin.test.Test
import kotlin.test.assertTrue

/**
 * A tripwire (docs/openwrt.md): Ktor encodes UTF-8 through glibc's iconv, which the static binary can't load, and
 * that is the only reason `limen join` uses `HttpLite` instead of Ktor's client. The day this fails, Ktor no longer
 * needs iconv for UTF-8: switch `limen join` to Ktor's client and delete HttpLite.
 */
class KtorCharsetTest {
    @Test
    fun ktorStillNeedsIconvForUtf8() {
        // Creating the encoder works (glibc has UTF-8 built in); converting text goes through UTF-16, a gconv module.
        val result =
            runCatching {
                Charsets.UTF_8
                    .newEncoder()
                    .encode("Ñandú")
                    .readByteArray()
            }
        assertTrue(
            result.isFailure,
            "Ktor's UTF-8 works in the static binary now: use Ktor's HTTP client in limen join and remove HttpLite (docs/openwrt.md)",
        )
    }
}
