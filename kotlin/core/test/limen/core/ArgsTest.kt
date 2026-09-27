package limen.core

import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class ArgsTest {
    private fun bad(block: () -> Unit): String {
        val e = assertFailsWith<LimenException> { block() }
        assertEquals(ErrorCode.BAD_REQUEST, e.code)
        return e.message!!
    }

    @Test
    fun defaultsAndTypes() {
        val args = Args.validate(Requests.LOGS.params, buildJsonObject { put("source", "unit") })
        assertEquals(JsonPrimitive(200), args["lines"])
        assertEquals("unit", args.string("source"))
    }

    @Test
    fun rejectsUnknownMissingAndWrongTypes() {
        assertEquals("unknown argument 'nmae'", bad { Args.validate(Requests.SERVICE.params, buildJsonObject { put("nmae", "x") }) })
        assertEquals("missing argument 'name'", bad { Args.validate(Requests.SERVICE.params, buildJsonObject { }) })
        assertEquals(
            "lines must be an integer",
            bad {
                Args.validate(
                    Requests.LOGS.params,
                    buildJsonObject {
                        put("source", "unit")
                        put("lines", "10")
                    },
                )
            },
        )
        assertEquals(
            "lines must be at most 10000",
            bad {
                Args.validate(
                    Requests.LOGS.params,
                    buildJsonObject {
                        put("source", "unit")
                        put("lines", 10001)
                    },
                )
            },
        )
        bad { Args.validate(Requests.LOGS.params, buildJsonObject { put("source", "shell") }) }
    }

    @Test
    fun patternsKeepShellAndPathTricksOut() {
        bad { Args.validate(Requests.SERVICE.params, buildJsonObject { put("name", "nginx; rm -rf /") }) }
        bad { Args.validate(Requests.CONTAINER_DETAIL.params, buildJsonObject { put("name", "--help") }) }
        bad { Args.validate(Requests.READ_FILE.params, buildJsonObject { put("path", "etc/passwd") }) }
        bad {
            Args.validate(
                Requests.LOGS.params,
                buildJsonObject {
                    put("source", "unit")
                    put("since", "yesterday")
                },
            )
        }
        Args.validate(
            Requests.LOGS.params,
            buildJsonObject {
                put("source", "unit")
                put("since", "2026-09-26T08:00:00Z")
            },
        )
    }

    @Test
    fun inputSchemaCarriesTheNodeAndTheRequired() {
        val node = Param("node", ParamType.ENUM, "Node", values = listOf("nas"))
        val schema = Args.inputSchema(Requests.SERVICE.params, listOf(node to true))
        assertEquals("""["node","name"]""", schema["required"].toString())
        assertEquals(JsonPrimitive("integer"), schema["properties"]!!.jsonObject["lines"]!!.jsonObject["type"])
    }
}
