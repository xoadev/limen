package limen.core

import kotlin.time.Duration
import kotlin.time.Duration.Companion.days
import kotlin.time.Duration.Companion.hours
import kotlin.time.Duration.Companion.milliseconds
import kotlin.time.Duration.Companion.minutes
import kotlin.time.Duration.Companion.seconds

/** `500ms`, `30s`, `5m`, `1h`, `2d`: how every duration in limen's files and arguments is written. */
object Durations {
    private val shape = Regex("^(\\d{1,9})(ms|s|m|h|d)$")

    fun parse(text: String): Duration? {
        val m = shape.matchEntire(text.trim()) ?: return null
        val n = m.groupValues[1].toLong()
        return when (m.groupValues[2]) {
            "ms" -> n.milliseconds
            "s" -> n.seconds
            "m" -> n.minutes
            "h" -> n.hours
            else -> n.days
        }
    }
}
