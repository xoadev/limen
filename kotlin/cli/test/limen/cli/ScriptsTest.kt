package limen.cli

import limen.cli.node.Node
import limen.cli.node.Scripts
import limen.cli.os.Fs
import limen.cli.os.Proc
import limen.core.config.NodeConfig
import limen.core.scripts.ScriptKind
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class ScriptsTest {
    private val dir = Proc.run(listOf("/bin/mktemp", "-d")).out.trim()

    @AfterTest
    fun removeTheTree() {
        Proc.run(listOf("/bin/rm", "-rf", dir))
    }

    @Test
    fun filesThatAreNotScriptsAreIgnoredNotProblems() {
        // A README, a .gitkeep to keep the folder in git: neither may stop `apply` or show up as a broken script.
        for (file in listOf("README.md", ".gitkeep")) Fs.writeAtomic("$dir/$file", "x\n".encodeToByteArray(), 0b110_100_100)
        val node = Node(NodeConfig(explicitChecks = dir, explicitSetup = dir))
        val entries = Scripts.discover(node, ScriptKind.SETUP)
        assertEquals(listOf("README.md"), entries.map { it.file })
        assertTrue(entries.single().ignored)
        assertEquals(emptyList(), Scripts.catalog(node).problems)
    }
}
