package limen.core

import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import limen.core.system.Parsers
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse

class ParsersTest {
    private val redactor = Redactor()

    @Test
    fun units() {
        val text =
            """
            accounts-daemon.service   loaded active running Accounts Service
            alsa-restore.service      loaded active exited  Save/Restore Sound Card State
            """.trimIndent()
        val units = Parsers.units(text)
        assertEquals(2, units.size)
        assertEquals(JsonPrimitive("exited"), units[1].jsonObject["sub"])
        assertEquals(JsonPrimitive("Save/Restore Sound Card State"), units[1].jsonObject["description"])
    }

    @Test
    fun unitShowWithUnsetValues() {
        val props =
            Parsers.keyValues(
                """
                Id=ssh.service
                LoadState=not-found
                MainPID=0
                MemoryCurrent=[not set]
                FragmentPath=
                NRestarts=2
                """.trimIndent(),
            )
        val unit = Parsers.unit(props)
        assertEquals(JsonPrimitive("not-found"), unit["load"])
        assertEquals(JsonNull, unit["main_pid"])
        assertEquals(JsonNull, unit["memory_bytes"])
        assertEquals(JsonNull, unit["unit_file"])
        assertEquals(JsonPrimitive(2L), unit["restarts"])
    }

    @Test
    fun journal() {
        val lines =
            """
            {"__REALTIME_TIMESTAMP":"1790440824182218","PRIORITY":"3","SYSLOG_IDENTIFIER":"app","_PID":"43","_SYSTEMD_UNIT":"app.service","MESSAGE":"login failed password=hunter2"}
            {"__REALTIME_TIMESTAMP":"1790440825000000","PRIORITY":"6","_COMM":"bin","MESSAGE":[104,105]}
            not json
            """.trimIndent()
        val entries = Parsers.journal(lines, redactor)
        assertEquals(2, entries.size)
        assertEquals(JsonPrimitive("2026-09-26T16:40:24Z"), entries[0]["time"])
        assertEquals(JsonPrimitive("err"), entries[0]["priority"])
        assertEquals(JsonPrimitive("login failed password=[redacted]"), entries[0]["message"])
        assertEquals(JsonPrimitive("hi"), entries[1]["message"])
        assertEquals(JsonPrimitive("bin"), entries[1]["source"])
    }

    @Test
    fun containerDetailHidesEnvironmentValues() {
        val inspect =
            Parsers
                .parseArray(
                    """
                    [{"Id":"0123456789abcdef","Name":"/immich","Image":"sha256:aa","Path":"start.sh","Args":["--token=abc"],
                      "State":{"Status":"running","Running":true,"ExitCode":0,
                        "Health":{"Status":"healthy","FailingStreak":0,"Log":[{"Start":"t","ExitCode":0,"Output":"ok\n"}]}},
                      "RestartCount":1,"HostConfig":{"RestartPolicy":{"Name":"unless-stopped"}},
                      "Config":{"Image":"ghcr.io/immich:v1","Env":["DB_PASSWORD=secret","TZ=UTC"],
                        "Labels":{"com.docker.compose.project":"photos"}},
                      "Mounts":[{"Type":"bind","Source":"/srv","Destination":"/data","RW":true}],
                      "NetworkSettings":{"Ports":{"80/tcp":null},"Networks":{"photos_default":{"IPAddress":"172.18.0.2"}}}}]
                    """.trimIndent(),
                )[0]
                .jsonObject
        val detail = Parsers.containerDetail(inspect, listOf("ghcr.io/immich@sha256:bb"), redactor)
        assertEquals("""["DB_PASSWORD","TZ"]""", detail["env"].toString())
        assertFalse("secret" in detail.toString())
        assertEquals(JsonPrimitive("start.sh --token=[redacted]"), detail["command"])
        assertEquals(JsonPrimitive("healthy"), detail["state"]!!.jsonObject["health"]!!.jsonObject["status"])
        assertEquals("0123456789ab", detail["id"]!!.jsonPrimitive.content)
        val summary = Parsers.containerSummary(inspect)
        assertEquals(JsonPrimitive("photos"), summary["compose_project"])
        assertEquals(JsonPrimitive("immich"), summary["name"])
    }

    @Test
    fun memoryAndOs() {
        val mem = Parsers.memory("MemTotal:       16000 kB\nMemAvailable:    8000 kB\nSwapTotal: 0 kB\n")
        assertEquals(JsonPrimitive(16000L * 1024), mem["total_bytes"])
        assertEquals("Debian GNU/Linux 13 (trixie)", Parsers.osName("ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"))
    }
}
