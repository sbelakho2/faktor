// Rich task tree: goal, acceptance criteria, plan/DAG steps with the
// children driving them, full child identity (pixel avatar, state, blocker,
// model/provider/reasoning, ownership/worktree, budget spend/remaining,
// progress, latest result), current phase, verification status, evidence
// list (selection = one-click retrieval) and durable spend. The blockers
// section and tournament view embed below the tree. Pure presentation: the
// model is built in TaskTreeModel.kt and all actions go to a Listener.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import java.awt.Font
import java.awt.GridLayout
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JSplitPane
import javax.swing.JTree
import javax.swing.event.TreeSelectionEvent
import javax.swing.tree.DefaultMutableTreeNode
import javax.swing.tree.DefaultTreeCellRenderer
import javax.swing.tree.DefaultTreeModel

/** Typed payloads the tree renders and routes on selection. */
sealed class TaskTreeNode(val label: String) {
    class Goal(label: String) : TaskTreeNode(label)
    class Child(val child: ChildNode) : TaskTreeNode("")
    class Evidence(val ref: EvidenceRef) : TaskTreeNode("")
    class Criterion(val row: CriterionProofRow) : TaskTreeNode("")
    class Plain(label: String) : TaskTreeNode(label)
}

/** Dimmed label color of background children (a presentation-only concept). */
private val BACKGROUND_DIM = java.awt.Color(140, 140, 140)

/** Verdict tones: pass (green), fail (red), unavailable (amber, italic). */
private val CRITERION_PASS_COLOR = java.awt.Color(63, 185, 80)
private val CRITERION_FAIL_COLOR = java.awt.Color(248, 81, 73)
private val CRITERION_UNAVAILABLE_COLOR = java.awt.Color(210, 153, 34)

class TaskTreePanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onEvidenceSelected(ref: EvidenceRef)
        fun onChildSelected(child: ChildNode)
    }

    private val stateLabel = JLabel("state: -")

    private val phaseLabel = JLabel("phase: -")

    private val spendLabel = JLabel("spend: -")

    private val verificationLabel = JLabel("verification: -")

    private val completionLabel = JLabel("completion: -")

    private val treeModel = DefaultTreeModel(DefaultMutableTreeNode(TaskTreeNode.Plain("no task data")))

    private val tree = JTree(treeModel)

    private var listener: Listener? = null

    private var currentModel: TaskTreeModel? = null

    init {
        tree.isRootVisible = true
        tree.showsRootHandles = true
        tree.cellRenderer = TaskTreeRenderer()
        tree.addTreeSelectionListener { event: TreeSelectionEvent ->
            val node = event.path.lastPathComponent as? DefaultMutableTreeNode ?: return@addTreeSelectionListener
            when (val payload = node.userObject) {
                is TaskTreeNode.Child -> listener?.onChildSelected(payload.child)
                is TaskTreeNode.Evidence -> listener?.onEvidenceSelected(payload.ref)
                else -> {}
            }
        }

        val summary = JPanel(GridLayout(0, 1, 2, 2))
        summary.add(stateLabel)
        summary.add(phaseLabel)
        summary.add(verificationLabel)
        summary.add(completionLabel)
        summary.add(spendLabel)

        val top = JPanel(BorderLayout(0, 4))
        top.add(summary, BorderLayout.NORTH)
        top.add(JScrollPane(tree), BorderLayout.CENTER)

        val split = JSplitPane(JSplitPane.VERTICAL_SPLIT, top, scroll(compactArea(4)))
        split.resizeWeight = 0.7
        split.isContinuousLayout = true
        add(split, BorderLayout.CENTER)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun model(): TaskTreeModel? = currentModel

    /** Rebuilds every section from one model; all values are real DTO fields. */
    fun update(model: TaskTreeModel) {
        currentModel = model
        stateLabel.text = "state: ${model.state}"
        phaseLabel.text = "phase: ${model.phase}"
        val verification = model.verification
        // The top-level summary: a served record that proves the full
        // criterion set with no failed check renders VERIFIED plus the served
        // criteria/checks/review/tree/commit/remote-head facts and the
        // durable spend's cost; otherwise the honest status counters stay.
        val costSuffix = model.spend?.let { " cost=${it.spentCostMicro}micro" } ?: ""
        verificationLabel.text = if (verification.verified) {
            "verification: " + verification.summaryText() + costSuffix
        } else {
            "verification: " + verification.summaryText() +
                " (criteria ${verification.criteriaPassed}/${verification.criteriaTotal}" +
                ", failed ${verification.failedChecks}, owed ${verification.owed})"
        }
        completionLabel.text = completionText(model.completion)
        spendLabel.text = spendText(model.spend)

        val root = DefaultMutableTreeNode(
            TaskTreeNode.Goal("goal: ${model.goal.ifEmpty { "(none)" }} [${model.state}]")
        )
        root.add(criteriaNode(model))
        root.add(proofNode(model))
        root.add(planNode(model))
        root.add(completionNode(model))
        root.add(childrenNode(model))
        root.add(verificationNode(model))
        root.add(evidenceNode(model))
        root.add(DefaultMutableTreeNode(TaskTreeNode.Plain(spendText(model.spend))))
        if (model.tournament != null) root.add(tournamentNode(model.tournament))
        treeModel.setRoot(root)
        for (row in 0 until tree.rowCount) tree.expandRow(row)
        tree.selectionModel.clearSelection()
    }

    private fun criteriaNode(model: TaskTreeModel): DefaultMutableTreeNode {
        // Per-criterion PROOF rows: verdict, requirement/origin, binding kind
        // with its exact reference, the proven snapshots, the verification
        // timestamps and the retrievable evidence refs. Rows are always
        // rendered from the served DTO facts; missing members stay explicit
        // "unavailable" text, never a fabricated pass.
        if (model.criteriaProof.isNotEmpty()) {
            val node = DefaultMutableTreeNode(
                TaskTreeNode.Plain("acceptance criteria · proof (${model.criteriaProof.size})")
            )
            for (row in model.criteriaProof) {
                val criterionNode = DefaultMutableTreeNode(TaskTreeNode.Criterion(row))
                node.add(criterionNode)
                for (ref in row.evidenceRefs) {
                    criterionNode.add(DefaultMutableTreeNode(TaskTreeNode.Evidence(ref)))
                }
            }
            return node
        }
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain("acceptance criteria (${model.acceptanceCriteria.size})")
        )
        for (criterion in model.acceptanceCriteria) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain(bound(criterion, 240))))
        }
        if (model.acceptanceCriteria.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(none served)")))
        }
        return node
    }

    /**
     * The top-level VERIFIED view from the strict proof summary. The daemon's
     * three-way verdict is preserved: `VERIFIED` renders only for `verified`;
     * an unavailable read lists its explicit component reasons and never the
     * verified verdict.
     */
    private fun proofNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val proof = model.proof
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain("verification proof: " + (proof?.summaryText() ?: "not fetched"))
        )
        if (proof == null) return node
        proof.reviewer?.takeIf { it.isNotEmpty() }?.let {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain(bound("reviewer: " + it, 240))))
        }
        proof.reason?.takeIf { it.isNotEmpty() }?.let {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain(bound("reason: " + it, 240))))
        }
        for (entry in proof.unavailable) {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(
                        bound(entry.component + " (" + entry.kind + "): " + entry.reason, 240)
                    )
                )
            )
        }
        for (line in proof.stepLines()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain(bound(line, 240))))
        }
        return node
    }

    private fun planNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(TaskTreeNode.Plain("plan / DAG steps (${model.steps.size})"))
        for (step in model.steps) {
            val label = StringBuilder()
            label.append("[").append(step.state).append("] ").append(step.id)
            if (step.summary.isNotEmpty() && step.summary != step.id) {
                label.append(" - ").append(step.summary)
            }
            if (step.dependsOn.isNotEmpty()) {
                label.append(" (depends on ").append(step.dependsOn.joinToString(",")).append(")")
            }
            if (step.childIds.isNotEmpty()) {
                label.append(" children=").append(step.childIds.joinToString(","))
            }
            val stepNode = DefaultMutableTreeNode(TaskTreeNode.Plain(bound(label.toString(), 240)))
            node.add(stepNode)
            for (childId in step.childIds) {
                val child = model.children.firstOrNull { it.childId == childId } ?: continue
                stepNode.add(DefaultMutableTreeNode(TaskTreeNode.Child(child)))
            }
        }
        if (model.steps.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(none served)")))
        }
        return node
    }

    private fun completionNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val completion = model.completion
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(completionText(completion))
        )
        if (completion == null) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(none: plain task, no commit/push/PR steps)")))
        } else {
            for (step in completion.steps) {
                val detail = if (step.detail == null) "" else " - ${step.detail}"
                node.add(
                    DefaultMutableTreeNode(
                        TaskTreeNode.Plain(bound("[${step.status}] ${step.step}$detail", 240))
                    )
                )
            }
        }
        return node
    }

    private fun childrenNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(TaskTreeNode.Plain("children (${model.children.size})"))
        for (child in model.children) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Child(child)))
        }
        if (model.children.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(no child agents)")))
        }
        return node
    }

    private fun verificationNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val summary = model.verification
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "verification: ${summary.status} criteria=${summary.criteriaPassed}/${summary.criteriaTotal}" +
                    " failedChecks=${summary.failedChecks} owed=${summary.owed}" +
                    (if (summary.recordStatus == null) "" else " record=${summary.recordStatus}")
            )
        )
        if (summary.recordStatus != null) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("record status: ${summary.recordStatus}")))
        }
        return node
    }

    private fun evidenceNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain("evidence (${model.evidence.size}) - select to retrieve")
        )
        for (ref in model.evidence) {
            val label = if (ref.id == null) {
                bound(ref.label, 200)
            } else {
                "evidence:${ref.id} ${bound(ref.label, 160)}"
            }
            val child = DefaultMutableTreeNode(TaskTreeNode.Evidence(ref))
            node.add(child)
        }
        if (model.evidence.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(no evidence refs)")))
        }
        return node
    }

    private fun tournamentNode(tournament: TournamentView): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "tournament ${tournament.id} [${tournament.state}] winner=${tournament.winner ?: "-"}"
            )
        )
        for (candidate in tournament.candidates) {
            val label = "${candidate.childId} [${candidate.state}]" +
                " verification=" + (candidate.verification?.let { "#$it" } ?: "-") +
                " review=" + (candidate.reviewRank ?: "-") +
                " cost=${candidate.costMicro}micro wall=${candidate.wallMs}ms" +
                (if (candidate.winner) " WINNER" else "")
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain(label)))
        }
        return node
    }

    private fun spendText(spend: SpendSummary?): String {
        if (spend == null) return "spend: -"
        val tokens = "${spend.spentTokens ?: 0}/${spend.maxTokens ?: "unlimited"}" +
            (spend.remainingTokens?.let { " remaining $it" } ?: "")
        val cost = "${spend.spentCostMicro ?: 0}/${spend.maxCostMicro ?: "unlimited"} micro" +
            (spend.remainingCostMicro?.let { " remaining $it" } ?: "")
        val open = if (spend.openReservedMicro > 0) " openReserved=${spend.openReservedMicro}" else ""
        return "spend (${if (spend.durable) "durable" else "estimate"}): tokens $tokens; cost $cost$open"
    }

    /** One summary line naming what was requested and the status provenance. */
    private fun completionText(completion: CompletionView?): String {
        if (completion == null) {
            return "completion: none (plain task)"
        }
        val requested = buildString {
            if (completion.includeCommit) append("commit")
            if (completion.includePush) {
                if (isNotEmpty()) append(",")
                append("push")
            }
            if (completion.includePr) {
                if (isNotEmpty()) append(",")
                append("pr")
            }
        }.ifEmpty { "none" }
        val statuses = completion.steps.joinToString(", ") { "${it.step}=${it.status}" }
        return "completion: $requested [$statuses] source=${completion.source}" +
            (completion.reason?.let { " ($it)" } ?: "")
    }

    /** The completion steps as tree rows (status + detail, never fabricated). */
    fun completionLabels(completion: CompletionView): List<String> =
        completion.steps.map { step ->
            val detail = if (step.detail == null) "" else " - ${step.detail}"
            bound("[${step.status}] ${step.step}$detail", 240)
        }

    /** The label of one child node, surfacing every native field. */
    fun childLabel(child: ChildNode): String {
        val text = StringBuilder()
        text.append(child.childId).append(" [").append(child.state).append("]")
        if (child.background) text.append(" (background)")
        if (child.itemId != null) text.append(" item=").append(child.itemId)
        if (child.itemKind != null) text.append(" kind=").append(child.itemKind)
        text.append(" model=").append(child.model ?: "-")
        text.append(" provider=").append(child.provider ?: "-")
        text.append(" reasoning=").append(
            if (child.reasoning == null) "-" else if (child.reasoning == true) "yes" else "no"
        )
        text.append(" ownership=").append(child.ownership)
        text.append(" worktree=").append(child.worktreeId)
        text.append(" tokens=").append(child.spentTokens ?: 0).append("/").append(child.budgetMaxTokens ?: "unlimited")
        if (child.remainingTokens != null) text.append(" remaining=").append(child.remainingTokens)
        text.append(" cost=").append(child.spentCostMicro ?: 0).append("/").append(child.maxCostMicro ?: "unlimited")
        child.blocker?.let { blocker ->
            text.append(" blocker=").append(blocker.kind).append(": ").append(bound(blocker.reason, 80))
        }
        child.progress?.let { progress ->
            text.append(" progress=").append(if (progress.stalled) "STALLED" else "live")
            if (progress.silenceMs != null) text.append("(").append(progress.silenceMs).append("ms)")
        }
        return text.toString()
    }

    /** The second line of one child node: latest result / merge envelope. */
    fun childResultLabel(child: ChildNode): String {
        val result = child.result ?: return "result: -"
        val text = StringBuilder("result: ").append(bound(result.summary, 160))
        result.merge?.let { merge ->
            text.append(" | merge ")
            if (merge.changeSetId != null) text.append(merge.changeSetId)
            text.append(" merged=").append(merge.merged ?: 0)
            text.append(" rejected=").append(merge.rejected ?: 0)
            text.append(" conflicts=").append(merge.conflicts ?: 0)
        }
        return text.toString()
    }

    /** The tone color of one criterion verdict (unavailable stays distinct). */
    fun criterionColor(tone: CriterionVerdictTone): java.awt.Color = when (tone) {
        CriterionVerdictTone.PASS -> CRITERION_PASS_COLOR
        CriterionVerdictTone.FAIL -> CRITERION_FAIL_COLOR
        CriterionVerdictTone.UNAVAILABLE -> CRITERION_UNAVAILABLE_COLOR
    }

    /**
     * The label of one criterion proof row, surfacing every member the wire
     * serves (verdict, requirement, origin, binding kind + exact reference,
     * the proven snapshots, the verification timestamps) plus the explicit
     * unavailable markers for members a predating daemon does not carry.
     * `[verdict]` leads so the state survives any bound.
     */
    fun criterionLabel(row: CriterionProofRow): String {
        val text = StringBuilder()
        text.append("[").append(row.verdict).append("] ")
        text.append(
            if (row.criterionKey.isEmpty()) "(unnamed criterion — malformed row)" else row.criterionKey
        )
        text.append(" · requirement ").append(row.requirement)
        text.append(" · origin ").append(row.origin)
        text.append(" · binding ").append(row.bindingKind)
        if (row.bindingSource == "derived") {
            text.append(" (").append(row.bindingSource).append(")")
        }
        row.bindingReference?.let { text.append(" ref ").append(it) }
        text.append(" · ").append(row.snapshot)
        text.append(" · ").append(row.verificationTimestamp)
        row.recordId?.let {
            text.append(" · record ").append(it).append(" [").append(row.recordStatus ?: "?").append("]")
        }
        row.verdictReason?.let { text.append(" · ").append(it) }
        if (row.unavailable.isNotEmpty()) {
            text.append(" · unavailable: ").append(row.unavailable.joinToString(", "))
        }
        return bound(text.toString(), 420)
    }

    /** The rendered labels of every proof row (smoke + inspection). */
    fun criterionLabels(model: TaskTreeModel): List<String> =
        model.criteriaProof.map { criterionLabel(it) }

    private inner class TaskTreeRenderer : DefaultTreeCellRenderer() {
        private var spriteId: String? = null
        private var sprite: PixelSprite? = null
        private val panel = JPanel(BorderLayout(4, 0))
        private val label = JLabel()

        override fun getTreeCellRendererComponent(
            tree: JTree?,
            value: Any?,
            selected: Boolean,
            expanded: Boolean,
            leaf: Boolean,
            row: Int,
            hasFocus: Boolean
        ): Component {
            val payload = ((value as? DefaultMutableTreeNode)?.userObject)
            if (payload is TaskTreeNode.Criterion) {
                val component = super.getTreeCellRendererComponent(
                    tree, value, selected, expanded, leaf, row, hasFocus
                )
                val proof = payload.row
                text = bound(criterionLabel(proof), 420)
                foreground = if (selected) textSelectionColor else criterionColor(proof.tone)
                font = font.deriveFont(
                    if (proof.tone == CriterionVerdictTone.UNAVAILABLE) Font.ITALIC else Font.PLAIN
                )
                return component
            }
            if (payload is TaskTreeNode.Child) {
                val child = payload.child
                if (spriteId != child.childId) {
                    spriteId = child.childId
                    sprite = PixelSprite(child.childId, child.state)
                    panel.removeAll()
                    paneAdd(sprite!!)
                    panel.add(label, BorderLayout.CENTER)
                }
                sprite?.setState(child.state)
                panel.background = if (selected) backgroundSelectionColor else backgroundNonSelectionColor
                panel.isOpaque = true
                // Background children are dimmed (gray + italic); presentation
                // never changes the child's state badge or controls.
                label.foreground = if (selected) {
                    textSelectionColor
                } else if (child.background) {
                    BACKGROUND_DIM
                } else {
                    textNonSelectionColor
                }
                label.font = label.font.deriveFont(if (child.background) Font.ITALIC else Font.PLAIN)
                label.text = bound(childLabel(child) + " || " + childResultLabel(child), 420)
                return panel
            }
            return super.getTreeCellRendererComponent(
                tree, value, selected, expanded, leaf, row, hasFocus
            )
        }

        private fun paneAdd(component: Component) {
            panel.add(component, BorderLayout.WEST)
        }
    }
}
