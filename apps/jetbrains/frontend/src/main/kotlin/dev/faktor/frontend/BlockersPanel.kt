// Dedicated blockers section: every blocked child with its blocker kind,
// reason, dependency, suggested resolution and its OWN inline action
// controls (Resume / Retry attached to that blocker card), plus the
// session's pending permission requests, each with its OWN Allow / Deny
// attached to that permission card. There is deliberately NO shared action
// bar and no hidden selection precedence: an action can never apply to a
// different item than the one it is drawn on. Pure presentation: actions are
// delivered to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.NativePermissionEntry
import java.awt.BorderLayout
import java.util.LinkedHashMap
import javax.swing.BorderFactory
import javax.swing.JButton
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextArea

class BlockersPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onBlockerAction(row: BlockerRow, action: BlockerAction)
        fun onPermissionReply(permission: NativePermissionEntry, decision: String)
    }

    private val cards = ScrollableColumn().apply {
        border = BorderFactory.createEmptyBorder(
            Spacing.M, Spacing.M, Spacing.M, Spacing.M
        )
    }

    private val taskBlockersArea = compactArea(2).apply {
        isFocusable = false
        isOpaque = false
        font = uiPanelFont()
    }

    private var blockers: List<BlockerRow> = emptyList()

    private var permissions: List<NativePermissionEntry>? = emptyList()

    private var taskBlockers: List<String> = emptyList()

    private val blockerButtons = LinkedHashMap<String, LinkedHashMap<BlockerAction, JButton>>()

    private class PermissionActions(val allow: JButton, val deny: JButton)

    private val permissionButtons = LinkedHashMap<String, PermissionActions>()

    private var listener: Listener? = null

    init {
        add(JScrollPane(cards), BorderLayout.CENTER)
        render()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the section contents from the task tree model. */
    fun update(model: TaskTreeModel) {
        update(model.blockers, emptyList(), model.taskBlockers)
    }

    /**
     * Blocked children + task blockers only. Pending approvals render as
     * their own contextual cards on the Agents view, so the approval section
     * is omitted here entirely rather than claiming "no pending permissions".
     */
    fun update(blockers: List<BlockerRow>, taskBlockers: List<String>) {
        this.blockers = blockers
        this.permissions = null
        this.taskBlockers = taskBlockers
        taskBlockersArea.text = if (taskBlockers.isEmpty()) {
            "No task-level blockers."
        } else {
            taskBlockers.joinToString("\n")
        }
        taskBlockersArea.foreground = if (taskBlockers.isEmpty()) {
            mutedForeground()
        } else {
            textForeground()
        }
        render()
    }

    fun update(
        blockers: List<BlockerRow>,
        permissions: List<NativePermissionEntry>,
        taskBlockers: List<String>
    ) {
        this.blockers = blockers
        this.permissions = permissions
        this.taskBlockers = taskBlockers
        taskBlockersArea.text = if (taskBlockers.isEmpty()) {
            "No task-level blockers."
        } else {
            taskBlockers.joinToString("\n")
        }
        taskBlockersArea.foreground = if (taskBlockers.isEmpty()) {
            mutedForeground()
        } else {
            textForeground()
        }
        render()
    }

    fun blockerCount(): Int = blockers.size

    fun permissionCount(): Int = permissions?.size ?: 0

    /**
     * The model's applicable actions (union over blocker rows) for smoke
     * observability. The UI renders each row's OWN controls inline; this is
     * never a shared selection-driven action set.
     */
    fun applicableActions(): List<BlockerAction> =
        blockers.flatMap { it.actions }.distinct()

    /** The inline Resume/Retry button attached to one blocker card. */
    internal fun blockerActionButton(childId: String, action: BlockerAction): JButton? =
        blockerButtons[childId]?.get(action)

    /** The inline Allow/Deny button attached to one permission card. */
    internal fun permissionActionButton(permissionId: String, decision: String): JButton? {
        val actions = permissionButtons[permissionId] ?: return null
        return if (decision == "allow") actions.allow else actions.deny
    }

    /** One card's rendered reason text (smoke observability). */
    internal fun blockerReasonText(childId: String): String? =
        blockers.firstOrNull { it.childId == childId }?.reason

    // ----------------------------------------------------------------- cards

    private fun render() {
        blockerButtons.clear()
        permissionButtons.clear()
        cards.removeAll()

        cards.add(sectionHeader("Blocked children (${blockers.size})"))
        if (blockers.isEmpty()) {
            cards.add(vSpace(Spacing.XS))
            cards.add(mutedLabel("No blocked children."))
        }
        for (row in blockers) {
            cards.add(vSpace(Spacing.S))
            cards.add(blockerCard(row))
        }

        cards.add(vSpace(Spacing.M))
        cards.add(sectionHeader("Task blockers (${taskBlockers.size})"))
        cards.add(vSpace(Spacing.XS))
        cards.add(taskBlockersArea)

        val permissionList = permissions
        if (permissionList != null) {
            cards.add(vSpace(Spacing.M))
            cards.add(sectionHeader("Pending permissions (${permissionList.size})"))
            if (permissionList.isEmpty()) {
                cards.add(vSpace(Spacing.XS))
                cards.add(mutedLabel("No pending permissions."))
            }
            for (permission in permissionList) {
                cards.add(vSpace(Spacing.S))
                cards.add(permissionCard(permission))
            }
        }

        cards.revalidate()
        cards.repaint()
    }

    private fun blockerCard(row: BlockerRow): JPanel {
        val info = WrappedLabel(
            buildString {
                append(row.childId).append(" [").append(row.presence.state.tag)
                    .append('/').append(row.kind).append("] ")
                append(bound(row.reason, 240))
                row.dependency?.let { append("\ndependency: ").append(bound(it, 160)) }
                row.resolution?.let { append("\nresolution: ").append(bound(it, 160)) }
                row.lastProgressMs?.let { append("\nlast progress: ").append(it).append("ms") }
            }
        )
        val card = card(null, info, hgap = Spacing.S, vgap = Spacing.S)
        val perCard = LinkedHashMap<BlockerAction, JButton>()
        // Permission allow/deny belongs to the PERMISSION card; the blocker
        // card carries only the child-level controls (resume/retry/cancel).
        val buttons = ArrayList<JButton>()
        for (action in row.actions.filter {
            it != BlockerAction.PERMISSION_ALLOW && it != BlockerAction.PERMISSION_DENY
        }) {
            val button = actionButton(action.label, primary = action == BlockerAction.RESUME)
            button.addActionListener { listener?.onBlockerAction(row, action) }
            perCard[action] = button
            buttons.add(button)
        }
        blockerButtons[row.childId] = perCard
        card.add(PixelSprite(row.childId, row.presence.state.tag), BorderLayout.WEST)
        if (buttons.isNotEmpty()) {
            card.add(actionRow(*buttons.toTypedArray()), BorderLayout.SOUTH)
        }
        return card
    }

    /** Human decision headline: what the agent wants to do, and to what. */
    private fun permissionHeadline(permission: NativePermissionEntry): String {
        val kind = permission.capability.lowercase()
        val what = when {
            kind.contains("shell") || kind.contains("execute") -> "Run a shell command"
            kind.contains("write") -> "Write files in this workspace"
            kind.contains("read") -> "Read files in this workspace"
            kind.contains("network") -> "Use the network"
            kind.contains("browser") -> "Control the browser"
            kind.contains("git") || kind.contains("scm") -> "Run Git operations"
            kind.contains("mcp") -> "Call an external tool service"
            else -> "Use ${permission.capability}"
        }
        val target = Regex("\"(?:tool|path|command|destination)\"\\s*:\\s*\"([^\"]+)\"")
            .find(permission.detail)
            ?.groupValues
            ?.get(1)
        return if (target.isNullOrEmpty()) what else "$what: $target"
    }

    private fun permissionCard(permission: NativePermissionEntry): JPanel {
        // Decision-first wording (UI audit): the headline says WHAT the
        // agent wants to do and to what; the reply handle and capability key
        // stay one muted diagnostic line below the Allow/Deny actions.
        val body = pageColumn(gap = Spacing.XS, padding = 0)
        body.add(WrappedLabel(permissionHeadline(permission)))
        if (permission.detail.isNotEmpty()) {
            body.add(mutedLabel(bound(permission.detail, 240)))
        }
        body.add(mutedLabel("#${permission.id} capability=${permission.capability}"))
        val info = body
        val allow = primaryButton("Allow")
        allow.addActionListener { listener?.onPermissionReply(permission, "allow") }
        val deny = secondaryButton("Deny")
        deny.addActionListener { listener?.onPermissionReply(permission, "deny") }
        permissionButtons[permission.id] = PermissionActions(allow, deny)
        val card = card(null, info, hgap = Spacing.S, vgap = Spacing.S)
        card.add(actionRow(allow, deny), BorderLayout.SOUTH)
        return card
    }
}
