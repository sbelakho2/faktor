// TEST-SEPARATION: oracle=external-fixture isolation=separate-process, read-only-artifacts
// The Ask-first product model: ONE composer intent (Ask default), ONE submit
// action, Task options behind one disclosure, cause-specific run states, a
// lazy Account surface, a concise Overview and developer-only diagnostics.
// Every test is scoped to the changed frontend surfaces.
package dev.faktor.frontend

import dev.faktor.shared.NativeMilestones
import dev.faktor.shared.NativeTaskView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import javax.swing.SwingUtilities
import java.nio.file.Files
import java.nio.file.Paths

class AskFirstUxTest {

    private fun panel(): FaktorChatPanel = FaktorChatPanel(
        FaktorFrontendService(
            Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-ask-first-ux-")
        )
    )

    @Test
    fun `default intent is Ask with one Send action and one Task options disclosure`() {
        val chat = panel()
        try {
            assertEquals(ComposerIntent.ASK, chat.intentForTest())
            assertTrue(chat.askModeSelected())
            assertEquals("Send", chat.submitLabelForTest())
            assertEquals(1, chat.submitActionCountForTest())
            assertFalse("Ask hides Task options", chat.taskOptionsVisibleForTest())
            assertEquals(false, chat.taskOptionsOpenForTest())

            SwingUtilities.invokeAndWait {
                chat.selectModeForTest(ask = false)
            }
            assertEquals(ComposerIntent.TASK, chat.intentForTest())
            assertEquals("Send", chat.submitLabelForTest())
            assertTrue("Task exposes ONE disclosure", chat.taskOptionsVisibleForTest())
            chat.openTaskOptionsForTest()
            assertTrue(chat.taskOptionsOpenForTest())
            chat.backTaskOptionsForTest()
            assertFalse(chat.taskOptionsOpenForTest())

            SwingUtilities.invokeAndWait {
                chat.selectModeForTest(ask = true)
            }
            assertFalse("Ask closes and hides Task options", chat.taskOptionsOpenForTest())
            assertFalse(chat.taskOptionsVisibleForTest())
        } finally {
            chat.shutdown()
        }
    }

    @Test
    fun `cause-specific states use the shared ids and never say recovery or resend`() {
        val mapping = listOf(
            UxSignals(credentialFailure = true) to UxStateKind.CREDENTIAL,
            UxSignals(providerRetrying = true) to UxStateKind.PROVIDER_RETRYING,
            UxSignals(daemonReconnecting = true) to UxStateKind.DAEMON_RECONNECTING,
            UxSignals(networkInterrupted = true) to UxStateKind.NETWORK_INTERRUPTED,
            UxSignals(paused = true) to UxStateKind.PAUSED,
            UxSignals(preRunFailed = true) to UxStateKind.PRE_RUN_FAILED,
            UxSignals(needsInput = true) to UxStateKind.NEEDS_INPUT,
            UxSignals(checkFailed = true) to UxStateKind.CHECK_FAILED,
            UxSignals(unrecoverable = true) to UxStateKind.UNRECOVERABLE
        )
        for ((signals, expected) in mapping) {
            val state = uxStateFor(signals)
            assertEquals("$signals must map to $expected", expected, state.kind)
            assertFalse(
                "no cause-specific state may use durable recovery/resend words: ${state.label}",
                state.label.contains("recovery", ignoreCase = true) ||
                    state.copy.contains("resend", ignoreCase = true) ||
                    state.copy.contains("recovery", ignoreCase = true)
            )
            assertTrue("every cause names one action", state.action != UxActionKind.NONE)
        }
        val all = listOf(
            UxStates.READY, UxStates.STARTING, UxStates.CREDENTIAL,
            UxStates.PROVIDER_RETRYING, UxStates.DAEMON_RECONNECTING,
            UxStates.NETWORK_INTERRUPTED, UxStates.PAUSED, UxStates.PRE_RUN_FAILED,
            UxStates.NEEDS_INPUT, UxStates.CHECK_FAILED, UxStates.UNRECOVERABLE,
            UxStates.RUNNING, UxStates.CHECKING, UxStates.CANCELLED, UxStates.FAILED
        )
        for (state in all) {
            for (text in listOf(state.label, state.copy)) {
                assertFalse(
                    "durable vocabulary belongs to Advanced: $text",
                    text.contains("durable", ignoreCase = true) ||
                        text.contains("recovery", ignoreCase = true) ||
                        text.contains("resend", ignoreCase = true)
                )
            }
        }
        assertEquals("Credential required", uxStateFor(UxSignals(credentialFailure = true)).label)
        assertEquals(
            "Connection interrupted",
            uxStateFor(UxSignals(networkInterrupted = true)).label
        )
        assertEquals("Resume", uxStateFor(UxSignals(paused = true)).actionLabel())
        assertEquals("Retry", uxStateFor(UxSignals(preRunFailed = true)).actionLabel())

        // Shared exact names/ids with the VS Code model.
        assertEquals(
            listOf("idle", "starting", "planning", "working", "awaiting_user",
                "checking", "done", "paused", "failed"),
            TaskPresentation.values().map { it.wireId }
        )
        assertEquals(
            listOf("approval", "failed_check", "missing_credential",
                "ambiguous_requirement", "repository_limitation"),
            AttentionItem.values().map { it.wireId }
        )
        assertEquals(
            TaskPresentation.AWAITING_USER,
            taskPresentationFor(UxSignals(needsInput = true))
        )
        assertEquals(
            TaskPresentation.FAILED,
            taskPresentationFor(UxSignals(preRunFailed = true))
        )
        assertEquals(TaskPresentation.DONE, taskPresentationFor(UxSignals(), done = true))
    }

    @Test
    fun `plain Ask has a conversation-only refresh and no account read`() {
        val chat = panel()
        try {
            val plan = chat.refreshReadsForTest()
            assertFalse("the run plan must not include Account", plan.contains("account"))
            assertFalse("the run plan must not include billing", plan.any { it.contains("billing") })
            assertFalse(chat.accountLoadedForTest())
            assertEquals(0, chat.accountReadsForTest())
        } finally {
            chat.shutdown()
        }
    }

    @Test
    fun `Account is lazy and says Account not connected without a control plane`() {
        val chat = panel()
        try {
            assertFalse(chat.accountLoadedForTest())
            assertTrue(chat.openUtilityForTest("Account"))
            assertTrue(chat.accountLoadedForTest())
            assertEquals(
                "a disconnected account must issue no identity/billing request",
                0,
                chat.accountReadsForTest()
            )
            assertEquals("Account not connected", chat.usageView().statusText())
        } finally {
            chat.shutdown()
        }
    }

    @Test
    fun `Overview aggregates attention to three, shows planned checks and omits absent sections`() {
        val blockers = listOf(
            "Approval needed for terminal",
            "Ambiguous requirement: which module?",
            "Repository limitation: generated files",
            "Credential missing for provider"
        )
        val task = NativeTaskView(
            goal = "concise overview",
            state = "failed",
            milestones = NativeMilestones(emptyList(), emptyList()),
            changedFiles = emptyList(),
            testsRun = emptyList(),
            testsFailed = emptyList(),
            budget = null,
            blockers = blockers
        )
        val model = TaskTree.build(task = task).copy(
            verification = TaskTree.build(task = task).verification.copy(
                checksPlanned = 4,
                failedChecks = 1
            )
        )
        val overview = OverviewPanel()
        overview.updateModel(model)
        assertTrue(overview.hasSummaryRowForTest("Needs attention"))
        val attention = overview.summaryText("Needs attention")
        assertTrue("attention must stay bounded: $attention", attention.split(" · ").size <= 3)
        assertTrue("overflow is aggregated: $attention", attention.contains("more"))
        assertEquals("Checks to run · 4", overview.summaryText("Checks to run"))
        assertFalse("full contract rows are Verification-only",
            overview.hasSummaryRowForTest("Acceptance criteria"))
        assertFalse(overview.hasSummaryRowForTest("Integration"))
        assertFalse(overview.hasSummaryRowForTest("Finish actions"))

        val clean = TaskTree.build(
            task = task.copy(state = "running", blockers = emptyList())
        )
        overview.updateModel(clean)
        assertFalse("absent attention is omitted", overview.hasSummaryRowForTest("Needs attention"))
        assertFalse("absent checks are omitted", overview.hasSummaryRowForTest("Checks to run"))
        assertFalse("absent agents are omitted", overview.hasSummaryRowForTest("Agents"))
        assertFalse("absent spend is omitted", overview.hasSummaryRowForTest("Spend"))
    }

    @Test
    fun `screen registry is absent from normal navigation and lives in Diagnostics Developer`() {
        val chat = panel()
        try {
            val labels = chat.normalNavigationLabelsForTest()
            assertFalse(
                "normal navigation must not expose the screen registry: $labels",
                labels.any {
                    it.contains("screen", ignoreCase = true) ||
                        it.contains("traceab", ignoreCase = true) ||
                        it.contains("registry", ignoreCase = true)
                }
            )
            assertEquals(
                listOf("Diagnostics", "Developer", "Screen registry…"),
                chat.developerRegistryMenuPathForTest()
            )
            assertTrue(chat.registryItemEnabledForTest())
        } finally {
            chat.shutdown()
        }
    }

    @Test
    fun `narrow identifiers ellipsize and composer controls keep accessible names`() {
        val identifier = "sha256:" + "a".repeat(64)
        val bounded = ellipsizeIdentifier(identifier, 24)
        assertTrue("identifier must stay within the bound: $bounded", bounded.length <= 24)
        assertTrue("the middle is elided: $bounded", bounded.contains("\u2026"))
        assertTrue("both ends stay visible: $bounded", bounded.startsWith("sha"))
        assertTrue(bounded.endsWith("a"))

        val chat = panel()
        try {
            ParityAwt.layout(chat, 240, 600)
            assertTrue(
                "Work must still use the narrow width: ${chat.workPlaneForTest().width}",
                chat.workPlaneForTest().width >= 200
            )
            assertTrue(
                "every composer control needs an accessible name: ${chat.composerA11yNamesForTest()}",
                chat.composerA11yNamesForTest().all { it.isNotBlank() }
            )
        } finally {
            chat.shutdown()
        }
    }

    @Test
    fun `overview updates stay bounded under a large served blocker set`() {
        val task = NativeTaskView(
            goal = "large attention",
            state = "failed",
            milestones = NativeMilestones(emptyList(), emptyList()),
            changedFiles = emptyList(),
            testsRun = emptyList(),
            testsFailed = emptyList(),
            budget = null,
            blockers = (1..500).map { "blocker $it" }
        )
        val model = TaskTree.build(task = task)
        val overview = OverviewPanel()
        val started = System.nanoTime()
        repeat(250) { index ->
            overview.updateModel(if (index % 2 == 0) model else model.copy(state = "running"))
        }
        val elapsedMs = (System.nanoTime() - started) / 1_000_000
        assertTrue("250 overview updates took ${elapsedMs}ms", elapsedMs < 5_000)
        assertTrue(overview.summaryText("Needs attention").split(" · ").size <= 3)
    }
}
