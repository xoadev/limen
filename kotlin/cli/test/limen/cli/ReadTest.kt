package limen.cli

import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import limen.cli.node.Node
import limen.cli.node.Read
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.core.ErrorCode
import limen.core.LimenException
import limen.core.config.NodeConfig
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class ReadTest {
    private val dir = Proc.run(listOf("/bin/mktemp", "-d")).out.trim()
    private val node =
        Node(
            NodeConfig(
                allow = listOf("$dir/etc/**"),
                deny = listOf("$dir/etc/private/**"),
                audit = "$dir/audit.jsonl",
            ),
        )

    init {
        Proc.run(listOf("/bin/mkdir", "-p", "$dir/etc/private", "$dir/outside"))
        write("etc/app.conf", "name = app\npassword = \"two words\"\n")
        write("etc/private/real", "x\n")
        write("outside/real", "x\n")
        write("etc/deploy.pem", "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n")
        Proc.run(listOf("/bin/ln", "-s", "$dir/outside/real", "$dir/etc/link"))
        Proc.run(listOf("/bin/ln", "-s", "$dir/etc/private", "$dir/etc/privlink"))
    }

    @AfterTest
    fun removeTheTree() {
        Proc.run(listOf("/bin/rm", "-rf", dir))
    }

    private fun write(
        path: String,
        text: String,
    ) = Fs.writeAtomic("$dir/$path", text.encodeToByteArray(), 0b110_100_100)

    private fun path(p: String): Map<String, JsonElement> = mapOf("path" to JsonPrimitive(p))

    private fun refused(block: () -> Unit): ErrorCode = assertFailsWith<LimenException> { block() }.code

    @Test
    fun anAllowedFileIsReadAndRedacted() {
        val content =
            Read
                .readFile(node, path("$dir/etc/app.conf"))
                .data.jsonObject["content"]!!
                .jsonPrimitive.content
        assertEquals("name = app\npassword = \"[redacted]\"", content)
    }

    @Test
    fun aDeniedPathSaysNothingOfItsExistence() {
        val denied =
            listOf("private/real", "private/missing", "../outside/real", "../outside/missing", "link", "privlink/real", "privlink/missing")
        for (denied in denied) {
            assertEquals(ErrorCode.DENIED, refused { Read.readFile(node, path("$dir/etc/$denied")) }, "read_file $denied")
            assertEquals(ErrorCode.DENIED, refused { Read.listDir(node, path("$dir/etc/$denied")) }, "list_dir $denied")
        }
        assertEquals(ErrorCode.NOT_FOUND, refused { Read.readFile(node, path("$dir/etc/missing")) })
        assertEquals(ErrorCode.BAD_REQUEST, refused { Read.listDir(node, path("$dir/etc/app.conf")) })
    }

    @Test
    fun listingHidesWhatCannotBeRead() {
        val names =
            Read
                .listDir(node, path("$dir/etc"))
                .data
                .jsonObject["entries"]!!
                .jsonArray
                .map { it.jsonObject["name"]!!.jsonPrimitive.content }
        assertTrue("app.conf" in names, names.toString())
        assertFalse("private" in names || "link" in names, names.toString())
    }

    @Test
    fun aFileHoldingAPrivateKeyIsNotReadAtAll() {
        // A window of lines between the markers would carry the key's body past redaction.
        val window = mapOf("path" to JsonPrimitive("$dir/etc/deploy.pem"), "from" to JsonPrimitive(2), "lines" to JsonPrimitive(2))
        assertEquals(ErrorCode.DENIED, refused { Read.readFile(node, window) })
    }

    @Test
    fun aKeyInABigLogIsMaskedWhenTheWindowHoldsIt() {
        // Over the size searched whole for keys: the window is redacted as one text, markers and body together.
        val filler = "x".repeat(99) + "\n"
        val key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n"
        write("etc/big.log", filler.repeat(12_000) + key + "done\n")
        val args = mapOf("source" to JsonPrimitive("file"), "name" to JsonPrimitive("$dir/etc/big.log"), "lines" to JsonPrimitive(10))
        val lines = Read.logs(node, args).data.toString()
        assertTrue("done" in lines, lines)
        assertFalse("b3BlbnNzaA" in lines, lines)
    }

    @Test
    fun historyIsRedacted() {
        write("audit.jsonl", """{"request":"action","args":{"name":"rotate","args":{"token":"abc123"}}}""" + "\n")
        val history = Read.history(node, emptyMap()).data.toString()
        assertTrue("rotate" in history, history)
        assertFalse("abc123" in history, history)
    }
}
