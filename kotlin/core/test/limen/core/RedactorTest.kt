package limen.core

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse

class RedactorTest {
    private val redactor = Redactor()

    @Test
    fun keepsTheKeyAndMasksTheValue() {
        assertEquals("password=[redacted] user=ana", redactor.redact("password=hunter2 user=ana"))
        assertEquals("DB_PASSWORD: [redacted]", redactor.redact("DB_PASSWORD: s3cr3t"))
        assertEquals("--api-key=[redacted] --verbose", redactor.redact("--api-key=abc123 --verbose"))
        assertEquals("\"token\": \"[redacted]\"", redactor.redact("\"token\": \"eyJhbGc\""))
    }

    @Test
    fun headersUrlsAndKeys() {
        assertEquals("Authorization: Bearer [redacted]", redactor.redact("Authorization: Bearer eyJ.abc.def"))
        assertEquals("postgres://app:[redacted]@db:5432/x", redactor.redact("postgres://app:pa55@db:5432/x"))
        val pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz"
        assertEquals("a\n[redacted]\nz", redactor.redact(pem))
    }

    @Test
    fun leavesOrdinaryTextAlone() {
        val text = "Started nginx.service - A high performance web server."
        assertEquals(text, redactor.redact(text))
    }

    @Test
    fun extraPatternsWithAndWithoutGroup() {
        val r = Redactor(listOf("sk-[A-Za-z0-9]{8,}", "pin (?<secret>\\d{4})"))
        assertEquals("key [redacted] and pin [redacted]", r.redact("key sk-abcdefgh123 and pin 1234"))
        assertFalse("sk-abcdefgh123" in r.redact("sk-abcdefgh123"))
    }
}
