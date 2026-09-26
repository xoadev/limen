package limen.core

import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import limen.core.system.Parsers
import kotlin.test.Test
import kotlin.test.assertEquals

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
    fun disksSkipTheLocalisedHeader() {
        val text =
            """
            S.ficheros     Tipo bloques de 1B       Usados         Disp Montado en
            /dev/nvme0n1p2 ext4  983038173184 278574559232 654452559872 /
            /dev/nvme0n1p1 vfat     313942016      8138752    305803264 /boot/efi
            """.trimIndent()
        val disks = Parsers.disks(text)
        assertEquals(2, disks.size)
        assertEquals(JsonPrimitive("/boot/efi"), disks[1].jsonObject["mount"])
        assertEquals(JsonPrimitive(28.3), disks[0].jsonObject["used_percent"])
    }

    @Test
    fun ports() {
        val text =
            """
            udp UNCONN 0 0 127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=680,fd=14))
            tcp LISTEN 0 4096 [::]:22 [::]:* users:(("sshd",pid=1,fd=4),("sshd",pid=1,fd=5))
            """.trimIndent()
        val ports = Parsers.ports(text)
        assertEquals(JsonPrimitive(53), ports[0].jsonObject["port"])
        assertEquals(JsonPrimitive("[::]"), ports[1].jsonObject["address"])
        assertEquals(1, ports[1].jsonObject["processes"]!!.jsonArray.size)
    }

    @Test
    fun processes() {
        val text = " 808340  1000 49.4 16.9 5418456  3481 /usr/bin/app --password=x -p 1\n"
        val p = Parsers.processes(text, { if (it == 1000) "ana" else null }, redactor)[0].jsonObject
        assertEquals(JsonPrimitive("ana"), p["user"])
        assertEquals(JsonPrimitive(5418456L * 1024), p["rss_bytes"])
        assertEquals(JsonPrimitive("/usr/bin/app --password=[redacted] -p 1"), p["command"])
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
