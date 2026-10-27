// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
// The redesign's structural contract: Work keeps one compact status row and
// an anchored composer, run details move to Inspect, and Settings is one
// searchable, domain-grouped surface with an explicit save model.
package dev.faktor.frontend

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.awt.Component
import java.awt.Container
import java.awt.image.BufferedImage
import java.nio.file.Files
import java.nio.file.Paths
import javax.imageio.ImageIO

class ChatSettingsUxTest {

    @Test
    fun `settings home paints a real searchable surface at 480 by 900`() {
        val panel = Edition8SettingsHomePanel(Edition8Host())
        panel.setSize(480, 900)
        layoutRecursively(panel)
        val image = BufferedImage(480, 900, BufferedImage.TYPE_INT_ARGB)
        val graphics = image.createGraphics()
        try {
            panel.paint(graphics)
        } finally {
            graphics.dispose()
        }
        val out = Paths.get("build", "screenshots", "ux-settings-home-480.png")
        Files.createDirectories(out.parent)
        ImageIO.write(image, "png", out.toFile())
        assertTrue("the painted surface must not be blank", Files.size(out) > 2048)
    }

    private fun layoutRecursively(container: Container) {
        container.doLayout()
        for (child in container.components) {
            if (child is Container) layoutRecursively(child) else child.doLayout()
        }
    }

    @Test
    fun `work exposes only state run tokens and approvals above the transcript`() {
        val panel = FaktorChatPanel(
            FaktorFrontendService(
                binaryPath = Paths.get("faktor"),
                dataDir = Files.createTempDirectory("faktor-ux")
            )
        )
        val texts = panel.workStatusTextForTest()
        assertEquals("Idle", texts[0])
        assertEquals("No active run", texts[1])
        assertTrue("tokens slot must be explicit: ${texts[2]}", texts[2].startsWith("Tokens"))
        assertEquals("No approvals", texts[3])
        assertEquals(4, panel.workStatusComponentsForTest().size)
        assertNotNull("run details must be hosted outside Work", panel.runDetailsPanelForTest())
        assertNotNull(panel.workDetailsButtonForTest())
        assertNotNull(panel.workStopButtonForTest())
        assertNotNull(panel.workCenterForTest())
    }

    @Test
    fun `settings home is one searchable domain grouped surface`() {
        val panel = Edition8SettingsHomePanel(Edition8Host())
        assertEquals(
            listOf(
                "Providers and models",
                "Context and compaction",
                "Tools and MCP",
                "Permissions",
                "Billing and usage",
                "Appearance and general"
            ),
            panel.domainLabelsForTest()
        )
        val total = panel.totalRowCountForTest()
        assertTrue("every settings screen must be reachable: $total", total >= 20)
        assertEquals(total, panel.visibleRowCountForTest())
        assertTrue(
            "the save model must be explicit: ${panel.saveStateTextForTest()}",
            panel.saveStateTextForTest().contains("Apply")
        )
        panel.searchFieldForTest().text = "mcp"
        assertTrue(panel.visibleRowCountForTest() in 1 until total)
        panel.searchFieldForTest().text = "no such setting"
        assertEquals(0, panel.visibleRowCountForTest())
        panel.searchFieldForTest().text = ""
        assertEquals(total, panel.visibleRowCountForTest())
    }
}
