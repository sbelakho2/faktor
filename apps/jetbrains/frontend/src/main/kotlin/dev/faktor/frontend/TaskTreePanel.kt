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
import java.awt.Dimension
import java.awt.Font
import java.awt.Point
import java.math.BigInteger
import java.util.LinkedHashMap
import java.util.LinkedHashSet
import javax.swing.BorderFactory
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTree
import javax.swing.event.TreeSelectionEvent
import javax.swing.tree.DefaultMutableTreeNode
import javax.swing.tree.DefaultTreeCellRenderer
import javax.swing.tree.DefaultTreeModel
import javax.swing.tree.TreePath

/**
 * Typed payloads the tree renders and routes on selection. Every node carries
 * a stable [nodeKey] so an in-place update can diff old and new rows without
 * disturbing expansion, selection or scroll.
 */
sealed class TaskTreeNode(val label: String, val nodeKey: String) {
    class Goal(label: String) : TaskTreeNode(label, "goal")
    class Child(val child: ChildNode) : TaskTreeNode("", "child:" + child.childId)
    class Evidence(val ref: EvidenceRef) :
        TaskTreeNode("", "evidence:" + (ref.id?.toString() ?: ref.label))
    class Criterion(val row: CriterionProofRow) :
        TaskTreeNode("", "criterion:" + row.criterionKey)
    class Plain(label: String, nodeKey: String) : TaskTreeNode(label, nodeKey)
}

/** Verdict tones derive from the live theme (never raw RGB). */

class TaskTreePanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onEvidenceSelected(ref: EvidenceRef)
        fun onChildSelected(child: ChildNode)
    }

    private val stateLabel = JLabel("-")

    private val phaseLabel = JLabel("-")

    private val spendLabel = WrappedLabel("not loaded")

    private val verificationLabel = WrappedLabel("not loaded")

    private val completionLabel = WrappedLabel("not loaded")

    private val treeModel = DefaultTreeModel(
        DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "No task data yet. Start a task to populate the tree.",
                "placeholder"
            )
        )
    )

    private val tree = JTree(treeModel)

    private val treeScroll = insetScroll(tree)

    /** The row under the pointer (rollover is renderer-only, never state). */
    private var hoverRow = -1

    /** One detail region for the selected node (no nested dead split pane). */
    private val detailArea = compactArea(3).apply {
        isFocusable = false
        preferredSize = Dimension(200, 116)
    }

    private val detailScroll = insetScroll(detailArea)

    private var listener: Listener? = null

    private var currentModel: TaskTreeModel? = null

    /** Suppresses action dispatch while an update restores selection. */
    private var suppressSelectionEvents = false

    /** Captured UI state keyed by stable node ids across one update. */
    private data class TreeUiState(
        val expandedKeys: Set<String>,
        val selectedKey: String?,
        val scrollPosition: Point?
    )

    init {
        tree.isRootVisible = true
        tree.showsRootHandles = true
        tree.rowHeight = Math.max(24, tree.getFontMetrics(uiPanelFont()).height + 10)
        tree.cellRenderer = TaskTreeRenderer()
        tree.addMouseMotionListener(
            object : java.awt.event.MouseMotionAdapter() {
                override fun mouseMoved(e: java.awt.event.MouseEvent?) {
                    val point = e?.point ?: return
                    val row = tree.getRowForLocation(point.x, point.y)
                    if (row != hoverRow) {
                        hoverRow = row
                        tree.repaint()
                    }
                }
            }
        )
        tree.addMouseListener(
            object : java.awt.event.MouseAdapter() {
                override fun mouseExited(e: java.awt.event.MouseEvent?) {
                    if (hoverRow != -1) {
                        hoverRow = -1
                        tree.repaint()
                    }
                }
            }
        )
        treeScroll.horizontalScrollBarPolicy =
            javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
        tree.addTreeSelectionListener { event: TreeSelectionEvent ->
            updateDetail()
            if (suppressSelectionEvents) return@addTreeSelectionListener
            val node = event.path.lastPathComponent as? DefaultMutableTreeNode
                ?: return@addTreeSelectionListener
            when (val payload = node.userObject) {
                is TaskTreeNode.Child -> listener?.onChildSelected(payload.child)
                is TaskTreeNode.Evidence -> listener?.onEvidenceSelected(payload.ref)
                else -> {}
            }
        }

        val summary = FormGrid()
            .row("State", stateLabel)
            .row("Phase", phaseLabel)
            .row("Verification", verificationLabel)
            .row("Completion", completionLabel)
            .row("Spend", spendLabel)
            .build()

        val top = JPanel(BorderLayout(0, Spacing.M))
        top.border = BorderFactory.createEmptyBorder(Spacing.M, Spacing.M, Spacing.M, Spacing.M)
        top.isOpaque = true
        top.background = panelSurface()
        top.add(card("Plan summary", summary), BorderLayout.NORTH)
        top.add(treeScroll, BorderLayout.CENTER)
        top.add(card("Selected node", detailScroll), BorderLayout.SOUTH)
        add(top, BorderLayout.CENTER)
        updateDetail()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun model(): TaskTreeModel? = currentModel

    /** The selected node detail (empty when nothing is selected). */
    internal fun selectedDetailText(): String = detailArea.text

    internal fun treeForTest(): JTree = tree

    internal fun treeScrollForTest(): JScrollPane = treeScroll

    /** The stable key path of one tree node (smoke observability). */
    internal fun nodeKeyPathForTest(node: DefaultMutableTreeNode): String = keyPath(node)

    /**
     * In-place diff update keyed by stable node ids: every section/row keeps
     * its node object when its key still exists, so user expansion state,
     * selection and (best effort) the scroll offset survive a data refresh.
     * The rejected alternative — rebuilding the root and expanding every row —
     * reset all three on every refresh.
     */
    fun update(model: TaskTreeModel) {
        currentModel = model
        stateLabel.text = model.state
        phaseLabel.text = model.phase
        val verification = model.verification
        // The top-level summary: a served record that proves the full
        // criterion set with no failed check renders VERIFIED plus the served
        // criteria/checks/review/tree/commit/remote-head facts and the
        // durable spend's cost; otherwise the honest status counters stay.
        val costSuffix = model.spend?.let { " cost=${it.spentCostMicro}micro" } ?: ""
        verificationLabel.fullText = if (verification.verified) {
            verification.summaryText() + costSuffix
        } else {
            verification.summaryText() +
                " (criteria ${verification.criteriaPassed}/${verification.criteriaTotal}" +
                ", failed ${verification.failedChecks}, owed ${verification.owed})"
        }
        completionLabel.fullText = completionText(model.completion)
        spendLabel.fullText = spendText(model.spend)

        val desired = DefaultMutableTreeNode(
            TaskTreeNode.Goal("goal: ${model.goal.ifEmpty { "(none)" }} [${model.state}]")
        )
        desired.add(criteriaNode(model))
        desired.add(proofNode(model))
        desired.add(planNode(model))
        desired.add(completionNode(model))
        desired.add(childrenNode(model))
        desired.add(verificationNode(model))
        desired.add(evidenceNode(model))
        desired.add(DefaultMutableTreeNode(TaskTreeNode.Plain(spendText(model.spend), "section:spend")))
        if (model.tournament != null) desired.add(tournamentNode(model.tournament))

        val state = captureUiState()
        val existingRoot = treeModel.root as? DefaultMutableTreeNode
        suppressSelectionEvents = true
        try {
            if (existingRoot == null || existingRoot.userObject !is TaskTreeNode.Goal) {
                treeModel.setRoot(desired)
                // First build: present the sections open (later updates never
                // re-expand rows the operator collapsed).
                for (row in 0 until tree.rowCount) tree.expandRow(row)
            } else {
                mergeNode(existingRoot, desired)
                restoreUiState(state)
            }
        } finally {
            suppressSelectionEvents = false
        }
        updateDetail()
    }

    // ------------------------------------------------------- in-place diff

    private fun keyOf(node: DefaultMutableTreeNode): String =
        (node.userObject as? TaskTreeNode)?.nodeKey ?: node.userObject?.toString() ?: "node"

    private fun keyPath(node: DefaultMutableTreeNode): String {
        val parts = ArrayList<String>()
        var current: DefaultMutableTreeNode? = node
        while (current != null) {
            var key = keyOf(current)
            val parent = current.parent as? DefaultMutableTreeNode
            if (parent != null) {
                var duplicates = 0
                for (index in 0 until parent.childCount) {
                    val sibling = parent.getChildAt(index) as DefaultMutableTreeNode
                    if (sibling === current) break
                    if (keyOf(sibling) == key) duplicates++
                }
                if (duplicates > 0) key = "$key#$duplicates"
            }
            parts.add(key)
            current = parent
        }
        return parts.asReversed().joinToString("/")
    }

    private fun captureUiState(): TreeUiState {
        val expanded = LinkedHashSet<String>()
        for (row in 0 until tree.rowCount) {
            val path = tree.getPathForRow(row) ?: continue
            val node = path.lastPathComponent as? DefaultMutableTreeNode ?: continue
            if (tree.isExpanded(path)) expanded.add(keyPath(node))
        }
        val selected = (tree.selectionPath?.lastPathComponent as? DefaultMutableTreeNode)
            ?.let { keyPath(it) }
        val viewPosition = treeScroll.viewport.viewPosition
        return TreeUiState(
            expanded,
            selected,
            if (viewPosition == null) null else Point(viewPosition)
        )
    }

    private fun restoreUiState(state: TreeUiState) {
        val root = treeModel.root as? DefaultMutableTreeNode ?: return
        if (state.expandedKeys.isNotEmpty()) {
            walk(root) { node ->
                if (!node.isLeaf && state.expandedKeys.contains(keyPath(node))) {
                    tree.expandPath(TreePath(node.path))
                }
            }
        }
        val selectedKey = state.selectedKey
        if (selectedKey != null) {
            val target = findFirst(root) { keyPath(it) == selectedKey }
            if (target != null) {
                tree.selectionPath = TreePath(target.path)
            }
        }
        state.scrollPosition?.let { treeScroll.viewport.viewPosition = it }
    }

    private fun walk(node: DefaultMutableTreeNode, visit: (DefaultMutableTreeNode) -> Unit) {
        visit(node)
        for (index in 0 until node.childCount) {
            walk(node.getChildAt(index) as DefaultMutableTreeNode, visit)
        }
    }

    private fun findFirst(
        node: DefaultMutableTreeNode,
        match: (DefaultMutableTreeNode) -> Boolean
    ): DefaultMutableTreeNode? {
        if (match(node)) return node
        for (index in 0 until node.childCount) {
            val found = findFirst(node.getChildAt(index) as DefaultMutableTreeNode, match)
            if (found != null) return found
        }
        return null
    }

    /** Updates [existing] from [desired], reusing matching children by key. */
    private fun mergeNode(existing: DefaultMutableTreeNode, desired: DefaultMutableTreeNode) {
        if (existing.userObject != desired.userObject) {
            existing.userObject = desired.userObject
        }
        mergeChildren(existing, desired)
    }

    private fun mergeChildren(existing: DefaultMutableTreeNode, desired: DefaultMutableTreeNode) {
        val buckets = LinkedHashMap<String, ArrayDeque<DefaultMutableTreeNode>>()
        for (index in 0 until existing.childCount) {
            val child = existing.getChildAt(index) as DefaultMutableTreeNode
            buckets.getOrPut(keyOf(child)) { ArrayDeque() }.addLast(child)
        }
        val ordered = ArrayList<DefaultMutableTreeNode>(desired.childCount)
        for (index in 0 until desired.childCount) {
            val wanted = desired.getChildAt(index) as DefaultMutableTreeNode
            val reused = buckets[keyOf(wanted)]?.removeFirstOrNull()
            if (reused != null) {
                mergeNode(reused, wanted)
                ordered.add(reused)
            } else {
                ordered.add(wanted)
            }
        }
        for (bucket in buckets.values) {
            for (dead in bucket) {
                if (dead.parent != null) treeModel.removeNodeFromParent(dead)
            }
        }
        val currentOrder = ArrayList<DefaultMutableTreeNode>(existing.childCount)
        for (index in 0 until existing.childCount) {
            currentOrder.add(existing.getChildAt(index) as DefaultMutableTreeNode)
        }
        if (currentOrder != ordered) {
            for (child in currentOrder) {
                if (child.parent != null) treeModel.removeNodeFromParent(child)
            }
            for ((index, child) in ordered.withIndex()) {
                treeModel.insertNodeInto(child, existing, index)
            }
        }
    }

    private fun updateDetail() {
        val node = tree.selectionPath?.lastPathComponent as? DefaultMutableTreeNode
        detailArea.text = if (node == null) {
            "Select a node to inspect its details."
        } else {
            detailText(node)
        }
    }

    private fun detailText(node: DefaultMutableTreeNode): String =
        when (val payload = node.userObject) {
            is TaskTreeNode.Child ->
                childLabel(payload.child) + "\n" + childResultLabel(payload.child)
            is TaskTreeNode.Evidence -> evidenceLabel(payload.ref)
            is TaskTreeNode.Criterion -> criterionLabel(payload.row)
            is TaskTreeNode.Goal -> payload.label
            is TaskTreeNode.Plain -> payload.label
            else -> ""
        }

    /** One plain row label (never the Kotlin class@hash fallback). */
    private fun nodeLabel(payload: TaskTreeNode): String = when (payload) {
        is TaskTreeNode.Goal -> payload.label
        is TaskTreeNode.Plain -> payload.label
        is TaskTreeNode.Evidence -> evidenceLabel(payload.ref)
        else -> payload.label
    }

    private fun evidenceLabel(ref: EvidenceRef): String {
        val id = ref.id ?: return ref.label
        // The label often already carries the same marker; never double it.
        return if (EvidenceRefs.parse(ref.label).id == id) ref.label else "evidence:$id " + ref.label
    }

    private fun criteriaNode(model: TaskTreeModel): DefaultMutableTreeNode {
        // Per-criterion PROOF rows: verdict, requirement/origin, binding kind
        // with its exact reference, the proven snapshots, the verification
        // timestamps and the retrievable evidence refs. Rows are always
        // rendered from the served DTO facts; missing members stay explicit
        // "unavailable" text, never a fabricated pass.
        if (model.criteriaProof.isNotEmpty()) {
            val node = DefaultMutableTreeNode(
                TaskTreeNode.Plain(
                    "acceptance criteria · proof (${model.criteriaProof.size})",
                    "section:criteria"
                )
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
            TaskTreeNode.Plain(
                "acceptance criteria (${model.acceptanceCriteria.size})",
                "section:criteria"
            )
        )
        for (criterion in model.acceptanceCriteria) {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(bound(criterion, 240), "criterion-text:" + bound(criterion, 240))
                )
            )
        }
        if (model.acceptanceCriteria.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(none served)", "criteria:none")))
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
            TaskTreeNode.Plain(
                "verification proof: " + (proof?.summaryText() ?: "not fetched"),
                "section:proof"
            )
        )
        if (proof == null) return node
        proof.reviewer?.takeIf { it.isNotEmpty() }?.let {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(bound("reviewer: " + it, 240), "proof:reviewer")
                )
            )
        }
        proof.reason?.takeIf { it.isNotEmpty() }?.let {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(bound("reason: " + it, 240), "proof:reason")
                )
            )
        }
        for (entry in proof.unavailable) {
            val line = bound(entry.component + " (" + entry.kind + "): " + entry.reason, 240)
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(line, "proof:unavailable:" + entry.component)
                )
            )
        }
        for (line in proof.stepLines()) {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(bound(line, 240), "proof:line:" + bound(line, 80))
                )
            )
        }
        return node
    }

    private fun planNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain("plan / DAG steps (${model.steps.size})", "section:plan")
        )
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
            val stepNode = DefaultMutableTreeNode(
                TaskTreeNode.Plain(bound(label.toString(), 240), "step:" + step.id)
            )
            node.add(stepNode)
            for (childId in step.childIds) {
                val child = model.children.firstOrNull { it.childId == childId } ?: continue
                stepNode.add(DefaultMutableTreeNode(TaskTreeNode.Child(child)))
            }
        }
        if (model.steps.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(none served)", "plan:none")))
        }
        return node
    }

    private fun completionNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val completion = model.completion
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(completionText(completion), "section:completion")
        )
        if (completion == null) {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(
                        "(none: plain task, no commit/push/PR steps)",
                        "completion:none"
                    )
                )
            )
        } else {
            for (step in completion.steps) {
                val detail = if (step.detail == null) "" else " - ${step.detail}"
                node.add(
                    DefaultMutableTreeNode(
                        TaskTreeNode.Plain(
                            bound("[${step.status}] ${step.step}$detail", 240),
                            "completion-step:" + step.step
                        )
                    )
                )
            }
        }
        return node
    }

    private fun childrenNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain("children (${model.children.size})", "section:children")
        )
        for (child in model.children) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Child(child)))
        }
        if (model.children.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(no child agents)", "children:none")))
        }
        return node
    }

    private fun verificationNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val summary = model.verification
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "verification: ${summary.status} criteria=${summary.criteriaPassed}/${summary.criteriaTotal}" +
                    " failedChecks=${summary.failedChecks} owed=${summary.owed}" +
                    (if (summary.recordStatus == null) "" else " record=${summary.recordStatus}"),
                "section:verification"
            )
        )
        if (summary.recordStatus != null) {
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(
                        "record status: ${summary.recordStatus}",
                        "verification:record"
                    )
                )
            )
        }
        return node
    }

    private fun evidenceNode(model: TaskTreeModel): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "evidence (${model.evidence.size}) - select to retrieve",
                "section:evidence"
            )
        )
        for (ref in model.evidence) {
            val child = DefaultMutableTreeNode(TaskTreeNode.Evidence(ref))
            node.add(child)
        }
        if (model.evidence.isEmpty()) {
            node.add(DefaultMutableTreeNode(TaskTreeNode.Plain("(no evidence refs)", "evidence:none")))
        }
        return node
    }

    private fun tournamentNode(tournament: TournamentView): DefaultMutableTreeNode {
        val node = DefaultMutableTreeNode(
            TaskTreeNode.Plain(
                "tournament ${tournament.id} [${tournament.state}] winner=${tournament.winner ?: "-"}",
                "section:tournament"
            )
        )
        for (candidate in tournament.candidates) {
            val label = "${candidate.childId} [${candidate.state}]" +
                " verification=" + (candidate.verification?.let { "#$it" } ?: "-") +
                " review=" + (candidate.reviewRank ?: "-") +
                " cost=${candidate.costMicro}micro wall=${candidate.wallMs}ms" +
                (if (candidate.winner) " WINNER" else "")
            node.add(
                DefaultMutableTreeNode(
                    TaskTreeNode.Plain(label, "tournament:candidate:" + candidate.childId)
                )
            )
        }
        return node
    }

    private fun spendText(spend: SpendSummary?): String {
        if (spend == null) return "spend: -"
        val tokens = "${spend.spentTokens ?: 0}/${spend.maxTokens ?: "unlimited"}" +
            (spend.remainingTokens?.let { " remaining $it" } ?: "")
        val cost = "${spend.spentCostMicro ?: BigInteger.ZERO}/" +
            "${spend.maxCostMicro ?: "unlimited"} micro" +
            (spend.remainingCostMicro?.let { " remaining $it" } ?: "")
        val open = if (spend.openReservedMicro.signum() > 0) {
            " openReserved=${spend.openReservedMicro}"
        } else {
            ""
        }
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
        text.append(" cost=").append(child.spentCostMicro ?: BigInteger.ZERO)
            .append("/").append(child.maxCostMicro ?: "unlimited")
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
        CriterionVerdictTone.PASS -> semanticForeground(SemanticState.POSITIVE)
        CriterionVerdictTone.FAIL -> semanticForeground(SemanticState.NEGATIVE)
        CriterionVerdictTone.UNAVAILABLE -> semanticForeground(SemanticState.WARNING)
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
                if (!selected && row == hoverRow) {
                    background = FaktorTheme.hover()
                }
                val proof = payload.row
                val full = criterionLabel(proof)
                text = bound(full, 300)
                toolTipText = full
                foreground = if (selected) textSelectionColor else criterionColor(proof.tone)
                font = uiPanelFont().deriveFont(
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
                panel.background = when {
                    selected -> backgroundSelectionColor
                    row == hoverRow -> FaktorTheme.hover()
                    else -> backgroundNonSelectionColor
                }
                panel.isOpaque = true
                // Background children are dimmed (gray + italic); presentation
                // never changes the child's state badge or controls.
                label.foreground = if (selected) {
                    textSelectionColor
                } else if (child.background) {
                    semanticForeground(SemanticState.DIM)
                } else {
                    textNonSelectionColor
                }
                label.font = uiPanelFont().deriveFont(if (child.background) Font.ITALIC else Font.PLAIN)
                val full = childLabel(child) + " || " + childResultLabel(child)
                label.text = bound(full, 300)
                label.toolTipText = full
                panel.toolTipText = full
                return panel
            }
            return super.getTreeCellRendererComponent(
                tree, value, selected, expanded, leaf, row, hasFocus
            ).also {
                font = uiPanelFont()
                toolTipText = null
                if (!selected && row == hoverRow) {
                    background = FaktorTheme.hover()
                }
                if (payload is TaskTreeNode) {
                    val full = nodeLabel(payload)
                    text = bound(full, 300)
                    toolTipText = full
                    val section = payload is TaskTreeNode.Goal ||
                        (payload is TaskTreeNode.Plain && payload.nodeKey.startsWith("section:"))
                    if (section) font = sectionTitleFont()
                }
            }
        }

        private fun paneAdd(component: Component) {
            panel.add(component, BorderLayout.WEST)
        }
    }
}
