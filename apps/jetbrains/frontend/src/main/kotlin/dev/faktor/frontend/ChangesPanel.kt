// Changes: the served candidate changes and the native immutable diff review
// (I03). Rows are derived from served data only (projection `filesChanged`,
// the current run's `NativeTaskView.changedFiles`, the verification records'
// `changedFiles` and their criteria bindings); every attached fact is a
// served relationship. The scope shows the candidate identity (raw hashes
// only behind the advanced toggle), Previous/Next change (bounded, disabled
// at the ends), Open source diff (the immutable basis through the host
// adapter — never a live workspace file), Open claim (selects the related
// criterion in Verification when one is served), Return to run and the S15
// `Recheck current basis` action. Keyboard reachable: row toggles are
// focusable and Alt+Up/Alt+Down move the selection.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import java.awt.event.ActionEvent
import java.awt.event.InputEvent
import java.awt.event.KeyEvent
import javax.swing.AbstractAction
import javax.swing.BoxLayout
import javax.swing.ButtonGroup
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JPanel
import javax.swing.KeyStroke

class ChangesPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onEvidenceSelected(ref: EvidenceRef)

        /** Execute the immutable-basis open for the selected change. */
        fun onOpenSourceDiff(model: ChangeReviewModel, rowIndex: Int?)

        /** Select the related criterion in the Verification surface. */
        fun onOpenClaim(criterionKey: String)

        /** Return to the Work tab (the run surface). */
        fun onReturnToRun()

        /** S15: re-read proof/projection and re-derive the conflict facts. */
        fun onRecheckBasis()
    }

    private val header = WrappedLabel("changes: -")

    private val candidateLabel = WrappedLabel("No candidate identity is served for this run.")

    private val candidateBasis = wrappedMutedLabel("")

    private val rawHashesArea = compactArea(8, monospace = true)

    private val rawHashesToggle = secondaryButton("Show raw hashes")

    private val rawHashesBody = JPanel(BorderLayout())

    private val rowsColumn = ScrollableColumn()

    /** Rebuilt with the rows so a refresh never accumulates group members. */
    private var rowGroup = ButtonGroup()

    private val rowButtons = ArrayList<FaktorToggleButton>()

    private val detailColumn = ScrollableColumn()

    private val emptyLabel = mutedLabel(CHANGES_EMPTY_COPY)

    private val unavailableHeadline = sectionHeader(CHANGES_UNAVAILABLE_LABEL)

    private val unavailableCopy = wrappedMutedLabel("")

    private val diffUnavailableHeadline = sectionHeader(DIFF_UNAVAILABLE_LABEL)

    private val diffUnavailableCopy = wrappedMutedLabel(DIFF_UNAVAILABLE_COPY)

    private val refusalLabel = wrappedMutedLabel("")

    private val conflictColumn = ScrollableColumn()

    private val previousButton = secondaryButton("Previous change")

    private val nextButton = secondaryButton("Next change")

    private val openButton = primaryButton("Open source diff")

    private val claimButton = secondaryButton("Open claim")

    private val returnButton = secondaryButton("Return to run")

    private val recheckButton = secondaryButton("Recheck current basis")

    private var listener: Listener? = null

    private var model: ChangeReviewModel? = null

    private var selectedRowIndex: Int = -1

    private var openStateCard: JComponent? = null

    private var diffUnavailableCard: JComponent? = null

    private var refusalCard: JPanel? = null

    private val contentStack = JPanel()

    init {
        rowsColumn.layout = BoxLayout(rowsColumn, BoxLayout.Y_AXIS)
        detailColumn.layout = BoxLayout(detailColumn, BoxLayout.Y_AXIS)
        conflictColumn.layout = BoxLayout(conflictColumn, BoxLayout.Y_AXIS)
        rawHashesArea.isEditable = false
        rawHashesArea.lineWrap = true
        rawHashesArea.wrapStyleWord = true
        rawHashesBody.isOpaque = false
        rawHashesBody.add(insetScroll(rawHashesArea), BorderLayout.CENTER)
        rawHashesBody.isVisible = false
        rawHashesToggle.addActionListener {
            rawHashesBody.isVisible = rawHashesToggle.isSelected
            rawHashesToggle.text = if (rawHashesToggle.isSelected) "Hide raw hashes" else "Show raw hashes"
            revalidate()
            repaint()
        }
        previousButton.addActionListener { selectRelative(-1) }
        nextButton.addActionListener { selectRelative(1) }
        openButton.addActionListener { submitOpen() }
        claimButton.addActionListener { submitClaim() }
        returnButton.addActionListener { listener?.onReturnToRun() }
        recheckButton.addActionListener { listener?.onRecheckBasis() }
        installKeyboardNavigation()

        val candidateBody = pageColumn(gap = Spacing.XS, padding = 0)
        candidateBody.add(candidateLabel)
        candidateBody.add(candidateBasis)
        candidateBody.add(actionRow(rawHashesToggle))
        candidateBody.add(rawHashesBody)

        val rowsBody = JPanel(BorderLayout())
        rowsBody.isOpaque = false
        rowsBody.add(rowsColumn, BorderLayout.NORTH)
        rowsBody.add(emptyLabel, BorderLayout.CENTER)

        val detailBody = pageColumn(gap = Spacing.XS, padding = 0)
        detailBody.add(detailColumn)

        val actionsBody = wrapRow(
            previousButton, nextButton, openButton, claimButton, returnButton, recheckButton
        )

        // The open-state cards (Diff unavailable / typed refusal) stay in ONE
        // explicit card slot: a successful open clears them.
        val stateSlot = JPanel(BorderLayout())
        stateSlot.isOpaque = false
        diffUnavailableCard = card(
            null,
            pageColumn(gap = Spacing.XS, padding = 0).apply {
                add(diffUnavailableHeadline)
                add(diffUnavailableCopy)
            }
        )
        val refusal = card(null, refusalLabel)
        refusalCard = refusal
        stateSlot.add(diffUnavailableCard, BorderLayout.NORTH)
        stateSlot.add(refusal, BorderLayout.CENTER)
        diffUnavailableCard?.isVisible = false
        refusal.isVisible = false
        openStateCard = stateSlot

        contentStack.layout = BoxLayout(contentStack, BoxLayout.Y_AXIS)
        contentStack.isOpaque = false
        contentStack.add(card("Candidate", candidateBody))
        contentStack.add(vSpace(Spacing.M))
        contentStack.add(card("Changes", rowsBody))
        contentStack.add(vSpace(Spacing.S))
        contentStack.add(stateSlot)
        contentStack.add(vSpace(Spacing.M))
        contentStack.add(card("Selected change", detailBody))
        contentStack.add(vSpace(Spacing.M))
        contentStack.add(actionsBody)
        contentStack.add(vSpace(Spacing.M))
        contentStack.add(card("Basis recheck", conflictColumn))

        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(contentStack), BorderLayout.CENTER)
        showConflictFacts(emptyList())
        update(ChangeReviewModel(basis = null, rows = emptyList()))
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the view with the served model (selection survives refresh). */
    fun update(value: ChangeReviewModel) {
        val preferredPath = model?.rows?.getOrNull(selectedRowIndex)?.path
        model = value
        rebuildRows(value, preferredPath)
        renderCandidate(value)
        renderDetail()
        clearOpenState()
        updateButtons()
        header.text = when {
            value.unavailable != null -> "changes: unavailable"
            else -> "changes: " + plural(value.rows.size, "change")
        }
        revalidate()
        repaint()
    }

    /**
     * Legacy/served-lists entry point (canned renders, session reset): rows
     * carry paths only; the candidate basis and attached refs stay absent
     * when no served relationship exists.
     */
    fun update(files: List<String>, evidence: List<EvidenceRef>) {
        val rows = files.map { path ->
            ChangeRow(
                path = path,
                changeKind = null,
                verificationRecordIds = emptyList(),
                criterionKeys = emptyList(),
                evidenceRefs = evidence,
                expectedDigest = null
            )
        }
        update(ChangeReviewModel(basis = null, rows = rows))
    }

    /** Explicit read-failure state with its reason; stale rows are cleared. */
    fun setUnavailable(reason: String) {
        update(ChangeReviewModel(basis = null, rows = emptyList(), unavailable = reason))
    }

    /** The exact I03 `Diff unavailable` in-panel state. */
    fun showDiffUnavailable() {
        refusalCard?.isVisible = false
        diffUnavailableCard?.isVisible = true
        openStateCard?.revalidate()
        openStateCard?.repaint()
    }

    /** One typed open refusal (invalid/foreign/unreadable evidence). */
    fun showOpenRefusal(reason: String) {
        refusalLabel.fullText = reason
        diffUnavailableCard?.isVisible = false
        refusalCard?.isVisible = true
        openStateCard?.revalidate()
        openStateCard?.repaint()
    }

    fun clearOpenState() {
        diffUnavailableCard?.isVisible = false
        refusalCard?.isVisible = false
        openStateCard?.revalidate()
        openStateCard?.repaint()
    }

    /** Renders the S15 re-derived conflict facts (never a rebase control). */
    fun showConflictFacts(facts: List<String>) {
        conflictColumn.removeAll()
        if (facts.isEmpty()) {
            conflictColumn.add(mutedLabel("No basis recheck has been run yet."))
        } else {
            for (fact in facts) {
                conflictColumn.add(wrappedMutedLabel(fact))
            }
        }
        conflictColumn.revalidate()
        conflictColumn.repaint()
    }

    fun currentModel(): ChangeReviewModel? = model

    fun conflictFacts(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until conflictColumn.componentCount) {
            val component = conflictColumn.getComponent(i)
            if (component is WrappedLabel && component.fullText.isNotEmpty()) out.add(component.fullText)
        }
        return out
    }

    // ------------------------------------------------------------- actions

    private fun submitOpen() {
        listener?.onOpenSourceDiff(model ?: return, selectedIndexOrNull())
    }

    private fun submitClaim() {
        val row = selectedRow() ?: return
        val key = row.criterionKeys.firstOrNull() ?: return
        listener?.onOpenClaim(key)
    }

    private fun selectRelative(delta: Int) {
        val rows = model?.rows ?: return
        if (rows.isEmpty()) return
        val target = (selectedRowIndex + delta).coerceIn(0, rows.size - 1)
        selectIndex(target)
    }

    private fun installKeyboardNavigation() {
        val map = getInputMap(JComponent.WHEN_ANCESTOR_OF_FOCUSED_COMPONENT)
        val actions = actionMap
        map.put(KeyStroke.getKeyStroke(KeyEvent.VK_DOWN, InputEvent.ALT_DOWN_MASK), "faktor.changeNext")
        actions.put("faktor.changeNext", object : AbstractAction() {
            override fun actionPerformed(event: ActionEvent?) = selectRelative(1)
        })
        map.put(KeyStroke.getKeyStroke(KeyEvent.VK_UP, InputEvent.ALT_DOWN_MASK), "faktor.changePrevious")
        actions.put("faktor.changePrevious", object : AbstractAction() {
            override fun actionPerformed(event: ActionEvent?) = selectRelative(-1)
        })
    }

    // ------------------------------------------------------------ rendering

    private fun rebuildRows(value: ChangeReviewModel, preferredPath: String?) {
        rowsColumn.removeAll()
        rowGroup = ButtonGroup()
        rowButtons.clear()
        emptyLabel.isVisible = value.unavailable == null && value.rows.isEmpty()
        unavailableHeadline.isVisible = value.unavailable != null
        unavailableCopy.isVisible = value.unavailable != null
        unavailableCopy.fullText = value.unavailable ?: ""
        for (index in value.rows.indices) {
            val row = value.rows[index]
            val button = FaktorToggleButton(rowButtonText(row))
            button.toolTipText = rowTooltip(row)
            button.alignmentX = Component.LEFT_ALIGNMENT
            button.addActionListener { selectIndex(index) }
            rowGroup.add(button)
            rowButtons.add(button)
            rowsColumn.add(button)
            rowsColumn.add(vSpace(Spacing.XS))
        }
        val target = when {
            value.rows.isEmpty() -> -1
            preferredPath != null -> value.rows.indexOfFirst { it.path == preferredPath }
                .takeIf { it >= 0 } ?: selectedRowIndex.coerceIn(0, value.rows.size - 1)
            else -> selectedRowIndex.coerceIn(0, value.rows.size - 1)
        }
        selectedRowIndex = target
        if (target >= 0) rowButtons[target].isSelected = true
        rowsColumn.revalidate()
    }

    private fun rowButtonText(row: ChangeRow): String {
        val facts = ArrayList<String>()
        row.changeKind?.let { facts.add(it) }
        if (row.verificationRecordIds.isNotEmpty()) {
            facts.add(plural(row.verificationRecordIds.size, "record"))
        }
        if (row.criterionKeys.isNotEmpty()) {
            facts.add(plural(row.criterionKeys.size, "claim"))
        }
        if (row.evidenceRefs.isNotEmpty()) {
            facts.add(plural(row.evidenceRefs.size, "ref"))
        }
        val suffix = if (facts.isEmpty()) "" else " [" + facts.joinToString(", ") + "]"
        return bound("\u2022 " + row.path + suffix, 72)
    }

    private fun rowTooltip(row: ChangeRow): String {
        val lines = ArrayList<String>()
        lines.add(row.path)
        row.changeKind?.let { lines.add("served change kind: $it") }
        if (row.verificationRecordIds.isNotEmpty()) {
            lines.add("verification records: " + row.verificationRecordIds.joinToString(", "))
        }
        if (row.criterionKeys.isNotEmpty()) {
            lines.add("claims: " + row.criterionKeys.joinToString(", "))
        }
        row.expectedDigest?.let { lines.add("expected digest: $it") }
        if (row.evidenceRefs.isNotEmpty()) {
            lines.add("evidence: " + row.evidenceRefs.joinToString(", ") { it.label })
        }
        return bound(lines.joinToString("\n"), 400)
    }

    private fun renderCandidate(value: ChangeReviewModel) {
        val basis = value.basis
        candidateLabel.fullText = basis?.candidateLabel
            ?: "No candidate identity is served for this run."
        candidateBasis.fullText = when {
            basis?.sourceDiffEvidenceId != null ->
                "source diff: evidence ${basis.sourceDiffEvidenceId} (immutable)"
            basis?.sourceDiffEvidenceRaw != null ->
                "source diff: served as \"${bound(basis.sourceDiffEvidenceRaw, 80)}\" (not an evidence id)"
            else -> "source diff: no immutable source diff evidence is served"
        }
        val detail = basis?.candidateDetail.orEmpty()
        rawHashesArea.text = detail.joinToString("\n")
        rawHashesToggle.isVisible = detail.isNotEmpty()
        if (detail.isEmpty()) {
            rawHashesBody.isVisible = false
            rawHashesToggle.isSelected = false
            rawHashesToggle.text = "Show raw hashes"
        }
    }

    private fun renderDetail() {
        detailColumn.removeAll()
        val row = selectedRow()
        if (row == null) {
            detailColumn.add(mutedLabel("Select a change to review its served facts."))
            detailColumn.revalidate()
            return
        }
        detailColumn.add(sectionHeader(row.path, muted = false))
        detailColumn.add(
            wrappedMutedLabel(
                if (row.changeKind == null) {
                    "change kind: not served for this change"
                } else {
                    "change kind: " + row.changeKind
                }
            )
        )
        if (row.verificationRecordIds.isNotEmpty()) {
            detailColumn.add(
                wrappedMutedLabel("verification records: " + row.verificationRecordIds.joinToString(", "))
            )
        } else {
            detailColumn.add(wrappedMutedLabel("verification records: none of the served records list this path"))
        }
        if (row.criterionKeys.isNotEmpty()) {
            detailColumn.add(wrappedMutedLabel("claims: " + row.criterionKeys.joinToString(", ")))
        }
        row.expectedDigest?.let {
            detailColumn.add(wrappedMutedLabel("expected digest: " + it))
        }
        if (row.evidenceRefs.isNotEmpty()) {
            val buttons = row.evidenceRefs.map { ref ->
                val label = ref.label.ifBlank { "Evidence" }
                val button = secondaryButton(bound(label, 32))
                button.toolTipText = label
                button.addActionListener { listener?.onEvidenceSelected(ref) }
                button
            }
            detailColumn.add(actionRow(*buttons.toTypedArray()))
        } else {
            detailColumn.add(wrappedMutedLabel("No evidence refs are attached to this change."))
        }
        detailColumn.revalidate()
    }

    private fun updateButtons() {
        val rows = model?.rows.orEmpty()
        previousButton.isEnabled = selectedRowIndex > 0
        nextButton.isEnabled = selectedRowIndex in 0 until rows.size - 1
        openButton.isEnabled = rows.isNotEmpty()
        claimButton.isEnabled = selectedRow()?.criterionKeys?.isNotEmpty() == true
        recheckButton.isEnabled = model?.unavailable == null
        returnButton.isEnabled = true
    }

    private fun selectedRow(): ChangeRow? = model?.rows?.getOrNull(selectedRowIndex)

    private fun selectedIndexOrNull(): Int? = selectedRowIndex.takeIf { it >= 0 }

    // ---------------------------------------------------------- observables

    fun fileCount(): Int = model?.rows?.size ?: 0

    fun rowCount(): Int = fileCount()

    fun filesText(): String = model?.rows?.joinToString("\n") { it.path } ?: ""

    fun rowPaths(): List<String> = model?.rows?.map { it.path } ?: emptyList()

    fun selectedIndex(): Int = selectedRowIndex

    fun selectedPath(): String? = selectedRow()?.path

    fun selectIndex(index: Int) {
        val rows = model?.rows ?: return
        if (index !in rows.indices) return
        selectedRowIndex = index
        if (index < rowButtons.size) rowButtons[index].isSelected = true
        renderDetail()
        updateButtons()
    }

    fun previousEnabled(): Boolean = previousButton.isEnabled

    fun nextEnabled(): Boolean = nextButton.isEnabled

    fun openEnabled(): Boolean = openButton.isEnabled

    fun claimEnabled(): Boolean = claimButton.isEnabled

    internal fun clickNext() = nextButton.doClick()

    internal fun clickPrevious() = previousButton.doClick()

    internal fun clickOpen() = openButton.doClick()

    internal fun clickClaim() = claimButton.doClick()

    internal fun clickReturn() = returnButton.doClick()

    internal fun clickRecheck() = recheckButton.doClick()

    fun candidateText(): String = candidateLabel.fullText

    fun basisText(): String = candidateBasis.fullText

    fun rawHashesText(): String = rawHashesArea.text

    fun unavailableReason(): String? = model?.unavailable

    fun unavailableHeadlineText(): String = unavailableHeadline.fullText

    fun emptyText(): String = emptyLabel.text

    fun diffUnavailableVisible(): Boolean = diffUnavailableCard?.isVisible == true

    fun diffUnavailableLabelText(): String = diffUnavailableHeadline.fullText

    fun diffUnavailableCopyText(): String = diffUnavailableCopy.fullText

    fun refusalText(): String = refusalLabel.fullText
}
