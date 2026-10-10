// Verification: criteria, checks and reviewer in one place, rendered from
// the plan-derived model. Each criterion keeps its verdict marker as TEXT
// (the tone only re-enforces it) and its binding kind/reference one muted
// line down; attached evidence refs route to the evidence detail through
// the shared listener. Exact proof state (verified snapshot, landed
// snapshot, hashes) stays on the model and is surfaced by the Plan view
// and Diagnostics rather than as headline copy here.
//
// The Contract card frames the criteria: the durable engineering contract
// (constraints/invariants/risks/proof obligations) with provenance,
// severity/state text and the "Needs disclosure" line whenever a
// high/unknown risk or an unsatisfied obligation is owed. A legacy daemon
// that serves no contract member reads as an explicit empty state.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import javax.swing.BoxLayout
import javax.swing.JPanel

class VerificationPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onEvidenceSelected(ref: EvidenceRef)
    }

    private val header = WrappedLabel("verification: -")

    private val summary = WrappedLabel("No verification has been recorded yet.")

    private val criteriaColumn = ScrollableColumn()

    private val contractColumn = ScrollableColumn()

    private var listener: Listener? = null

    private var criterionCount = 0

    init {
        criteriaColumn.layout = BoxLayout(criteriaColumn, BoxLayout.Y_AXIS)
        contractColumn.layout = BoxLayout(contractColumn, BoxLayout.Y_AXIS)
        val summaryBody = pageColumn(gap = Spacing.XS, padding = 0)
        summaryBody.add(summary)
        val contractBody = pageColumn(gap = Spacing.XS, padding = 0)
        contractBody.add(contractColumn)
        val body = pageColumn()
        body.add(card("Verification summary", summaryBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Contract", contractBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Criteria and checks", criteriaColumn))
        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        updateModel(null)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the view with the served model; null is an honest empty state. */
    fun updateModel(model: TaskTreeModel?) {
        criteriaColumn.removeAll()
        contractColumn.removeAll()
        renderContract(model?.contract)
        if (model == null) {
            criterionCount = 0
            header.text = "verification: no run yet"
            summary.fullText = "No verification has been recorded yet."
            criteriaColumn.add(
                mutedLabel("Criteria, checks and reviewer appear here once a run is verified.")
            )
            criteriaColumn.revalidate()
            return
        }
        val verification = model.verification
        criterionCount = model.criteriaProof.size
        header.text = "verification: " + if (verification.verified) "verified" else "not verified"
        summary.fullText = buildString {
            append(if (verification.verified) "Verified" else "Not verified yet")
            append(" · criteria ").append(verification.criteriaPassed)
            append(" of ").append(verification.criteriaTotal).append(" passed")
            append(" · ").append(plural(verification.failedChecks, "failed check"))
            append(" · ").append(plural(verification.owed, "owed check"))
            verification.recordStatus?.let { append(" · record ").append(it) }
            verification.reviewer?.let { append(" · reviewer ").append(it) }
        }
        if (model.criteriaProof.isEmpty()) {
            criteriaColumn.add(
                mutedLabel("No criterion proofs are served for this run yet.")
            )
        } else {
            for (row in model.criteriaProof) {
                criteriaColumn.add(vSpace(Spacing.S))
                criteriaColumn.add(criterionCard(row))
            }
        }
        criteriaColumn.revalidate()
        revalidate()
        repaint()
    }

    /**
     * The Contract card rows. The "Needs disclosure" line is text-first (the
     * core's own state tokens, e.g. `risk:high`); a malformed member and
     * refused rows are disclosed too, and a served-but-empty contract stays
     * an explicit empty state (never a fabricated claim).
     */
    private fun renderContract(contract: ContractView?) {
        if (contract == null) {
            contractColumn.add(mutedLabel("No contract is served for this run."))
            contractColumn.revalidate()
            return
        }
        val lines = contract.disclosureLines()
        if (lines.isNotEmpty()) {
            contractColumn.add(
                labelWithTone("Needs disclosure", SemanticState.WARNING, title = true)
            )
            for (line in lines) {
                contractColumn.add(
                    wrappedLabelWithTone(line, SemanticState.WARNING)
                )
            }
            contractColumn.add(vSpace(Spacing.S))
        }
        if (contract.isEmpty) {
            contractColumn.add(
                mutedLabel(
                    if (contract.malformed == null) {
                        "No contract entries are recorded for this run."
                    } else {
                        "No contract entries are rendered for this run."
                    }
                )
            )
            contractColumn.revalidate()
            return
        }
        statementGroup("Constraints", contract.constraints)
        statementGroup("Invariants", contract.invariants)
        if (contract.risks.isNotEmpty()) {
            contractColumn.add(sectionHeader("Risks", muted = true))
            for (risk in contract.risks) {
                contractColumn.add(
                    wrappedMutedLabel(
                        "• " + risk.description + " — " + risk.severity + " · " + risk.origin
                    )
                )
                risk.disclosure?.let {
                    contractColumn.add(
                        wrappedLabelWithTone("disclosure: " + it, SemanticState.WARNING)
                    )
                }
            }
        }
        if (contract.obligations.isNotEmpty()) {
            contractColumn.add(sectionHeader("Proof obligations", muted = true))
            for (row in contract.obligations) {
                contractColumn.add(
                    wrappedMutedLabel(
                        "• " + row.claim + " — " + row.evidence + " · " + row.stateText +
                            " · " + row.origin
                    )
                )
                for (ref in row.evidenceRefs) {
                    contractColumn.add(wrappedMutedLabel("evidence: " + ref))
                }
            }
        }
        contractColumn.revalidate()
    }

    private fun statementGroup(title: String, rows: List<ContractStatementRow>) {
        if (rows.isEmpty()) return
        contractColumn.add(sectionHeader(title, muted = true))
        for (row in rows) {
            contractColumn.add(wrappedMutedLabel("• " + row.statement + " — " + row.origin))
        }
    }

    private fun labelWithTone(
        text: String,
        state: SemanticState,
        title: Boolean = false
    ): WrappedLabel {
        val label = WrappedLabel(text)
        label.foreground = semanticForeground(state)
        if (title) label.font = sectionTitleFont()
        return label
    }

    private fun wrappedLabelWithTone(text: String, state: SemanticState): WrappedLabel {
        val label = WrappedLabel(text)
        label.foreground = semanticForeground(state)
        return label
    }

    private fun criterionCard(row: CriterionProofRow): JPanel {
        val body = pageColumn(gap = Spacing.XS, padding = 0)
        val marker = when (row.tone) {
            CriterionVerdictTone.PASS -> "[pass]"
            CriterionVerdictTone.FAIL -> "[fail]"
            CriterionVerdictTone.UNAVAILABLE -> "[unavailable]"
        }
        val title = WrappedLabel("$marker ${row.criterionKey}")
        title.font = sectionTitleFont()
        title.foreground = if (row.tone == CriterionVerdictTone.PASS) {
            semanticForeground(SemanticState.POSITIVE)
        } else if (row.tone == CriterionVerdictTone.FAIL) {
            semanticForeground(SemanticState.NEGATIVE)
        } else if (row.tone == CriterionVerdictTone.UNAVAILABLE) {
            semanticForeground(SemanticState.WARNING)
        } else {
            mutedForeground()
        }
        body.add(title)
        val binding = row.bindingReference?.let { " via $it" } ?: ""
        body.add(wrappedMutedLabel("${row.requirement} criterion · ${row.bindingKind}$binding"))
        row.verdictReason?.let { body.add(wrappedMutedLabel(bound(it, 240))) }
        if (row.evidenceRefs.isNotEmpty()) {
            val evidenceRow = JPanel(BorderLayout())
            evidenceRow.isOpaque = false
            evidenceRow.alignmentX = Component.LEFT_ALIGNMENT
            for (ref in row.evidenceRefs) {
                val button = secondaryButton(ref.label.ifBlank { "Evidence" })
                button.addActionListener { listener?.onEvidenceSelected(ref) }
                evidenceRow.add(button, BorderLayout.WEST)
            }
            body.add(evidenceRow)
        }
        return card(null, body, hgap = Spacing.S, vgap = Spacing.S)
    }

    fun criterionCount(): Int = criterionCount

    fun summaryText(): String = summary.fullText

    /** Every text line the Contract card rendered (smoke assertions). */
    fun contractLabels(): List<String> {
        val out = ArrayList<String>()
        collectLabels(contractColumn, out)
        return out
    }

    private fun collectLabels(component: Component, out: MutableList<String>) {
        if (component is WrappedLabel && component.fullText.isNotEmpty()) {
            out.add(component.fullText)
        }
        if (component is java.awt.Container) {
            for (child in component.components) collectLabels(child, out)
        }
    }

    /** The rendered criterion verdict markers (state is never color-only). */
    fun criterionLabels(): List<String> {
        val out = ArrayList<String>()
        for (index in 0 until criteriaColumn.componentCount) {
            val component = criteriaColumn.getComponent(index) as? JPanel ?: continue
            collectMarkers(component, out)
        }
        return out
    }

    private fun collectMarkers(component: Component, out: MutableList<String>) {
        if (component is WrappedLabel && component.fullText.startsWith("[")) {
            out.add(component.fullText)
        }
        if (component is java.awt.Container) {
            for (child in component.components) collectMarkers(child, out)
        }
    }
}
