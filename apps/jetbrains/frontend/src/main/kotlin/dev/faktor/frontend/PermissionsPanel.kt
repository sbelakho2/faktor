// Pending approvals, rendered contextually: every pending permission is a
// "Faktor needs your approval" card whose headline names the requesting
// action and target, whose Allow/Deny buttons attach to THAT request, and
// whose Details disclosure carries the reply handle (request id), the owning
// session and the capability key. There is deliberately no standalone
// Permissions destination: the cards live next to the agents that asked.
// Pure presentation: the decision is delivered to a Listener; an unavailable
// route is recorded truthfully (never rendered as "no permissions").
package dev.faktor.frontend

import dev.faktor.shared.NativePermissionEntry
import java.awt.BorderLayout
import java.util.LinkedHashMap
import javax.swing.JButton
import javax.swing.JPanel

class PermissionsPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onPermissionReply(permission: NativePermissionEntry, decision: String)

        fun onRefresh()
    }

    private val header = WrappedLabel("Faktor needs your approval")

    private val cards = ScrollableColumn()

    private val refreshButton = secondaryButton("Refresh approvals")

    private val buttons = LinkedHashMap<String, Pair<JButton, JButton>>()

    private var listener: Listener? = null

    private var permissions: List<NativePermissionEntry> = emptyList()

    private var available = true

    private var reason: String? = null

    /** The last typed reply refusal (409 conflict / session mismatch), if any. */
    private var refusal: String? = null

    private var selectedId: String? = null

    private val detailArea = compactArea(3, monospace = true)

    init {
        cards.layout = javax.swing.BoxLayout(cards, javax.swing.BoxLayout.Y_AXIS)
        refreshButton.addActionListener { listener?.onRefresh() }
        val detailBody = JPanel(BorderLayout(0, Spacing.S))
        detailBody.isOpaque = false
        detailBody.add(insetScroll(detailArea), BorderLayout.CENTER)
        val body = pageColumn()
        body.add(card("Pending requests", cards))
        body.add(vSpace(Spacing.M))
        body.add(card("Request details", detailBody))
        body.add(vSpace(Spacing.M))
        body.add(actionRow(refreshButton))
        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        render()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the pending cards; a still-served selection is preserved. */
    fun update(permissionList: List<NativePermissionEntry>) {
        available = true
        reason = null
        refusal = null
        permissions = permissionList
        if (selectedId == null || permissionList.none { it.id == selectedId }) {
            selectedId = permissionList.firstOrNull()?.id
        }
        render()
    }

    /** The route failed (or is absent): record it instead of an empty list. */
    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        refusal = null
        permissions = emptyList()
        selectedId = null
        render()
    }

    /**
     * One reply was refused with the daemon's typed 409 (unknown/expired/
     * already resolved, or a waiter owned by another session). The pending
     * cards are kept and the refusal is recorded explicitly — never retried
     * behind the operator's back.
     */
    fun setReplyRefusal(detailText: String) {
        refusal = detailText
        render()
    }

    /** The last typed reply refusal, or null when the last view was clean. */
    fun refusalText(): String? = refusal

    fun count(): Int = permissions.size

    fun available(): Boolean = available

    fun selected(): NativePermissionEntry? =
        permissions.firstOrNull { it.id == selectedId } ?: permissions.firstOrNull()

    fun unitLabel(index: Int): String {
        val permission = permissions[index]
        return "#${permission.id} session=${permission.sessionId} " +
            "capability=${permission.capability} detail=${bound(permission.detail, 120)}"
    }

    fun allowEnabled(): Boolean = available && selected() != null

    fun denyEnabled(): Boolean = available && selected() != null

    fun headerText(): String = header.fullText

    fun detailText(): String {
        val permission = selected() ?: return if (!available) {
            reason ?: "approval route unavailable"
        } else {
            "No pending approval requests. New requests appear here for review."
        }
        return "#${permission.id}\nsession: ${permission.sessionId}" +
            "\ncapability: ${permission.capability}\ndetail: ${bound(permission.detail, 240)}"
    }

    /** Programmatic reply, as a click on Allow/Deny would produce. */
    fun submitReply(decision: String) {
        val permission = selected() ?: return
        if (!available) return
        listener?.onPermissionReply(permission, decision)
    }

    /** Test hook: clicks the Allow/Deny button attached to one request card. */
    internal fun clickCard(permissionId: String, decision: String): Boolean {
        val pair = buttons[permissionId] ?: return false
        val button = if (decision == "allow") pair.first else pair.second
        if (!button.isEnabled) return false
        button.doClick()
        return true
    }

    private fun render() {
        buttons.clear()
        cards.removeAll()
        if (!available) {
            cards.add(mutedLabel("Approvals unavailable: " + (reason ?: "unknown")))
            detailArea.text = reason ?: ""
        } else if (permissions.isEmpty()) {
            cards.add(
                mutedLabel("No pending approvals. Requests appear here while an agent works.")
            )
            detailArea.text = detailText()
        } else {
            for (permission in permissions) {
                cards.add(vSpace(Spacing.S))
                cards.add(permissionCard(permission))
            }
            detailArea.text = detailText()
        }
        refreshButton.isEnabled = available
        val suffix = when {
            !available -> " (unavailable)"
            refusal != null -> " (last reply refused)"
            else -> " — " + plural(permissions.size, "pending request")
        }
        header.text = "Faktor needs your approval" + suffix
        cards.revalidate()
        cards.repaint()
        revalidate()
        repaint()
    }

    private fun permissionCard(permission: NativePermissionEntry): JPanel {
        val body = pageColumn(gap = Spacing.XS, padding = 0)
        val headline = WrappedLabel(permissionHeadline(permission))
        headline.font = sectionTitleFont()
        headline.foreground = textForeground()
        body.add(headline)
        if (permission.detail.isNotEmpty()) {
            body.add(wrappedMutedLabel(bound(permission.detail, 240)))
        }
        val details = WrappedLabel(detailsText(permission))
        details.font = monospacePanelFont()
        details.foreground = mutedForeground()
        // The details disclosure starts closed: the card leads with the human
        // request, and the request id / session / capability open on demand
        // (the Request details card mirrors the selected request).
        details.isVisible = false
        val detailsToggle = secondaryButton("Details")
        detailsToggle.addActionListener {
            details.isVisible = !details.isVisible
            detailsToggle.text = if (details.isVisible) "Hide details" else "Details"
            selectedId = permission.id
            detailArea.text = detailsText(permission)
            revalidate()
            repaint()
        }
        val allow = primaryButton("Allow")
        allow.addActionListener {
            if (available) listener?.onPermissionReply(permission, "allow")
        }
        val deny = secondaryButton("Deny")
        deny.addActionListener {
            if (available) listener?.onPermissionReply(permission, "deny")
        }
        buttons[permission.id] = Pair(allow, deny)
        allow.isEnabled = available
        deny.isEnabled = available
        val actions = actionRow(allow, deny, detailsToggle)
        body.add(actions)
        body.add(details)
        return card("Request #${permission.id}", body, hgap = Spacing.S, vgap = Spacing.S)
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
            else -> "Use " + permission.capability
        }
        val target = Regex("\"(?:tool|path|command|destination)\"\\s*:\\s*\"([^\"]+)\"")
            .find(permission.detail)
            ?.groupValues
            ?.get(1)
        return if (target.isNullOrEmpty()) what else "$what: $target"
    }

    private fun detailsText(permission: NativePermissionEntry): String =
        "session ${permission.sessionId} · capability ${permission.capability}\n" +
            bound(permission.detail, 240)
}
