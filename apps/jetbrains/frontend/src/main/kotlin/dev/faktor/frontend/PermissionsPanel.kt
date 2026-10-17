// Pending approvals, rendered contextually: every pending permission is a
// "Faktor needs your approval" card whose headline names the requesting
// action and target, whose subline carries the authority reason (or the
// exact fallback plus the exact payload), and whose Allow/Deny buttons attach
// to THAT request. The card builder, the reply gating and the refusal
// handling are SHARED with the bounded Work attention card
// (ApprovalCards.kt), so the two surfaces can never diverge. There is
// deliberately no standalone Permissions destination: the cards live next to
// the agents that asked. Pure presentation: the decision is delivered to a
// Listener; an unavailable route is recorded truthfully (never rendered as
// "no permissions").
package dev.faktor.frontend

import dev.faktor.shared.NativePermissionEntry
import java.awt.BorderLayout
import javax.swing.JButton
import javax.swing.JPanel

class PermissionsPanel(
    /**
     * The per-request in-flight gate shared with the Work attention card:
     * one reply per request id, whichever surface the click came from.
     */
    private val gate: ApprovalReplyGate = ApprovalReplyGate()
) : JPanel(BorderLayout()) {

    interface Listener {
        fun onPermissionReply(permission: NativePermissionEntry, decision: String)

        fun onRefresh()
    }

    private val header = WrappedLabel(APPROVAL_HEADLINE)

    private val cards = ScrollableColumn()

    private val refreshButton = secondaryButton("Refresh approvals")

    /** Per-request rendered controls (rebuilt by [render], keyed by id). */
    private val cardsById = LinkedHashMap<String, ApprovalRequestCard>()

    private var listener: Listener? = null

    private var permissions: List<NativePermissionEntry> = emptyList()

    private var available = true

    private var reason: String? = null

    private var appliedGateRevision = -1L

    /** Per-request typed reply refusals, pruned to still-pending ids. */
    private val refusals = LinkedHashMap<String, String>()

    private var selectedId: String? = null

    private val detailArea = compactArea(3, monospace = true)

    init {
        cards.layout = javax.swing.BoxLayout(cards, javax.swing.BoxLayout.Y_AXIS)
        refreshButton.addActionListener { listener?.onRefresh() }
        val detailBody = JPanel(BorderLayout(0, Spacing.S))
        detailBody.isOpaque = false
        detailBody.add(insetScroll(detailArea), BorderLayout.CENTER)
        val body = pageColumn()
        val supporting = wrappedMutedLabel(APPROVAL_SUPPORTING)
        supporting.accessibleContext.accessibleName = APPROVAL_SUPPORTING
        body.add(supporting)
        body.add(vSpace(Spacing.M))
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
        val gateRevision = gate.revision()
        if (available && reason == null && permissionList == permissions &&
            gateRevision == appliedGateRevision
        ) {
            return
        }
        appliedGateRevision = gateRevision
        available = true
        reason = null
        permissions = permissionList
        pruneRefusals()
        if (selectedId == null || permissionList.none { it.id == selectedId }) {
            selectedId = permissionList.firstOrNull()?.id
        }
        render()
    }

    /** The route failed (or is absent): record it instead of an empty list. */
    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        refusals.clear()
        permissions = emptyList()
        selectedId = null
        render()
    }

    /**
     * One reply was refused with the daemon's typed 409 (unknown/expired/
     * already resolved, or a waiter owned by another session). The pending
     * card is kept, re-enabled and shows the refusal reason; the reply is
     * never retried behind the operator's back.
     */
    fun setReplyRefusal(permissionId: String, detailText: String) {
        refusals[permissionId] = detailText
        render()
    }

    /** Convenience for the selected request (test/compat call sites). */
    fun setReplyRefusal(detailText: String) {
        val id = selectedId ?: permissions.firstOrNull()?.id ?: return
        setReplyRefusal(id, detailText)
    }

    /** The last typed reply refusal, or null when none is recorded. */
    fun refusalText(): String? = refusals.values.lastOrNull()

    fun count(): Int = permissions.size

    fun available(): Boolean = available

    fun selected(): NativePermissionEntry? =
        permissions.firstOrNull { it.id == selectedId } ?: permissions.firstOrNull()

    fun unitLabel(index: Int): String {
        val permission = permissions[index]
        return "#${permission.id} session=${permission.sessionId} " +
            "capability=${permission.capability} detail=${bound(permission.detail, 120)}"
    }

    fun allowEnabled(): Boolean = available && selectedCard()?.allowButton?.isEnabled == true

    fun denyEnabled(): Boolean = available && selectedCard()?.denyButton?.isEnabled == true

    /** True while a reply for [permissionId] is in flight (test seam). */
    fun isReplyPendingForTest(permissionId: String): Boolean = gate.pending(permissionId)

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
        val card = selectedCard() ?: return
        val button = if (decision == "allow") card.allowButton else card.denyButton
        if (!button.isEnabled) return
        button.doClick()
    }

    /** Test hook: clicks the Allow/Deny button attached to one request card. */
    internal fun clickCard(permissionId: String, decision: String): Boolean {
        val card = cardsById[permissionId] ?: return false
        val button = if (decision == "allow") card.allowButton else card.denyButton
        if (!button.isEnabled) return false
        button.doClick()
        return true
    }

    private fun selectedCard(): ApprovalRequestCard? {
        val permission = selected() ?: return null
        return cardsById[permission.id]
    }

    private fun pruneRefusals() {
        val live = HashSet<String>()
        for (permission in permissions) live.add(permission.id)
        val stale = refusals.keys.filter { !live.contains(it) }
        for (id in stale) refusals.remove(id)
    }

    private fun render() {
        cardsById.clear()
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
                val card = approvalRequestCard(
                    permission = permission,
                    gate = gate,
                    onReply = { decision -> listener?.onPermissionReply(permission, decision) },
                    onSelect = {
                        selectedId = permission.id
                        detailArea.text = detailsText(permission)
                    }
                )
                cardsById[permission.id] = card
                cards.add(
                    card("Request #${permission.id}", card.body, hgap = Spacing.S, vgap = Spacing.S)
                )
            }
            detailArea.text = detailText()
        }
        refreshButton.isEnabled = available
        val refused = refusals.isNotEmpty()
        val suffix = when {
            !available -> " (unavailable)"
            refused -> " (last reply refused)"
            else -> " — " + plural(permissions.size, "pending request")
        }
        header.text = APPROVAL_HEADLINE + suffix
        cards.revalidate()
        cards.repaint()
        revalidate()
        repaint()
    }

    private fun detailsText(permission: NativePermissionEntry): String =
        "session ${permission.sessionId} · capability ${permission.capability}\n" +
            bound(permission.detail, 240)
}
