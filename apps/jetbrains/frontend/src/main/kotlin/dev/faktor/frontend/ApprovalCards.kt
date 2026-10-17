// ONE approval renderer for both decision surfaces: the bounded Work
// attention card above the composer and the Inspect -> Agents -> Approvals
// list. The card copy, the per-request Allow/Deny/Details actions and the
// in-flight gating are shared here, so the two surfaces can never diverge.
//
// D14: the subline is the AUTHORITY reason of the payload (`reason`/`purpose`/
// `why` inside a JSON OBJECT) or the exact `Action requested for this run.`
// fallback plus the exact payload — a purpose is never inferred. The
// in-flight gate is keyed by request id and survives panel rebuilds; a
// typed refusal releases it, keeps the request visible with the refusal
// reason and offers a refresh (never an implicit retry).
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativePermissionEntry
import java.awt.BorderLayout
import java.util.Collections
import javax.swing.JButton
import javax.swing.JPanel

/** The exact approval card copy (state matrix). */
internal const val APPROVAL_HEADLINE = "Faktor needs your approval"

/** T10 supporting copy. */
internal const val APPROVAL_SUPPORTING = "Review the action, target and stated reason."

/** D14: the fallback subline when the payload carries no authority reason. */
internal const val APPROVAL_FALLBACK_REASON = "Action requested for this run."

/** The bounded Work attention card renders at most this many requests. */
internal const val MAX_WORK_APPROVAL_CARDS = 3

/** The JSON object members that may carry an AUTHORITY-provided reason. */
private val APPROVAL_REASON_KEYS = listOf("reason", "purpose", "why")

/** The target keys in decision-relevance order (command before tool name). */
private val APPROVAL_TARGET_KEYS = listOf("command", "path", "destination", "tool")

/**
 * The parsed detail payload, ONLY when the detail is a JSON object; anything
 * else (a plain string, a JSON array, malformed text) carries no authority
 * members at all.
 */
internal fun approvalDetailObject(detail: String): JsonValue.Obj? {
    val trimmed = detail.trim()
    if (!trimmed.startsWith("{")) return null
    return try {
        JsonCodec.parse(trimmed) as? JsonValue.Obj
    } catch (e: Exception) {
        null
    }
}

/**
 * The AUTHORITY reason of one native detail payload: an explicit
 * `reason`/`purpose`/`why` string member of a JSON OBJECT, surfaced verbatim
 * (bounded). Nothing is inferred from other fields, and a detail that is not
 * a JSON object returns null.
 */
internal fun approvalAuthorityReason(detail: String): String? {
    val obj = approvalDetailObject(detail) ?: return null
    for (key in APPROVAL_REASON_KEYS) {
        val value = (obj.fields[key] as? JsonValue.Str)?.value?.trim()
        if (value != null && value.isNotEmpty()) return bound(value, 400)
    }
    return null
}

/** The copy the reason line renders: authority reason or the exact fallback. */
internal fun approvalReasonText(permission: NativePermissionEntry): String =
    approvalAuthorityReason(permission.detail) ?: APPROVAL_FALLBACK_REASON

/**
 * The decision target of one request: the most decision-relevant JSON member
 * (`command`/`path`/`destination`/`tool`) when the detail is an object, else
 * the legacy string scan over the same keys. Empty stays missing.
 */
internal fun approvalTargetOf(permission: NativePermissionEntry): String? {
    val obj = approvalDetailObject(permission.detail)
    if (obj != null) {
        for (key in APPROVAL_TARGET_KEYS) {
            val value = (obj.fields[key] as? JsonValue.Str)?.value
            if (value != null && value.isNotEmpty()) return value
        }
        return null
    }
    return Regex("\"(?:command|path|destination|tool)\"\\s*:\\s*\"([^\"]+)\"")
        .find(permission.detail)
        ?.groupValues
        ?.get(1)
        ?.takeIf { it.isNotEmpty() }
}

/** Human decision headline: what the run wants to do, and to what. */
internal fun approvalHeadlineOf(permission: NativePermissionEntry): String {
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
    val target = approvalTargetOf(permission)
    return if (target.isNullOrEmpty()) what else "$what: $target"
}

/**
 * The per-request in-flight gate. `tryBegin` claims one request id BEFORE the
 * reply leaves the UI; a second click/Enter/burst is refused until the typed
 * result releases the id. The gate belongs to the owning panel (not to the
 * transient buttons), so a rebuild renders the same pending request disabled.
 */
class ApprovalReplyGate {

    private val inFlight = Collections.synchronizedSet(LinkedHashSet<String>())

    private val revision = java.util.concurrent.atomic.AtomicLong()

    /** Claims [id]; false when a reply for it is already in flight. */
    fun tryBegin(id: String): Boolean {
        synchronized(inFlight) {
            if (inFlight.contains(id)) return false
            inFlight.add(id)
            revision.incrementAndGet()
            return true
        }
    }

    /** Releases [id] after its typed result (success or refusal). */
    fun finish(id: String) {
        synchronized(inFlight) {
            if (inFlight.remove(id)) revision.incrementAndGet()
        }
    }

    /** Monotonic claim/finish revision: a rebuild observes gating changes. */
    fun revision(): Long = revision.get()

    fun pending(id: String): Boolean = synchronized(inFlight) { inFlight.contains(id) }

    fun pendingIds(): Set<String> = synchronized(inFlight) { LinkedHashSet(inFlight) }
}

/**
 * The rendered per-request controls, shared by both surfaces. [body] is the
 * request body (headline, capability/target, reason, exact payload, refusal);
 * the caller wraps it in whatever card chrome its surface uses.
 */
internal class ApprovalRequestCard(
    val body: JPanel,
    val allowButton: JButton,
    val denyButton: JButton,
    val detailsButton: JButton,
    val refreshButton: JButton?
) {
    /** Enables/disables both decision buttons for this exact request. */
    fun setPending(pending: Boolean) {
        allowButton.isEnabled = !pending
        denyButton.isEnabled = !pending
    }
}

/**
 * ONE request body. Both surfaces render the EXACT same copy and behavior:
 *  - headline: what the run wants to do and to it;
 *  - capability / target / conversation line;
 *  - the authority reason or `Action requested for this run.`;
 *  - the exact payload (never truncated, never a fabricated purpose);
 *  - a typed refusal line when the last reply was refused (the request stays
 *    visible and a refresh affordance is offered);
 *  - Allow / Deny / Details (and Refresh when the caller supplies it).
 *
 * The decision buttons gate on [gate] per request id; the first click claims
 * the id and disables BOTH buttons until [ApprovalReplyGate.finish].
 */
internal fun approvalRequestCard(
    permission: NativePermissionEntry,
    gate: ApprovalReplyGate,
    onReply: (String) -> Unit,
    onSelect: () -> Unit = {},
    onRefresh: (() -> Unit)? = null,
    refusal: String? = null
): ApprovalRequestCard {
    val headlineText = approvalHeadlineOf(permission)
    val target = approvalTargetOf(permission)
    val reason = approvalReasonText(permission)
    val body = pageColumn(gap = Spacing.XS, padding = 0)
    val headline = WrappedLabel(headlineText)
    headline.font = sectionTitleFont()
    headline.foreground = textForeground()
    headline.accessibleContext.accessibleName = "$APPROVAL_HEADLINE: $headlineText"
    body.add(headline)
    val infoBits = ArrayList<String>()
    infoBits.add(
        "capability " + (permission.capability.ifBlank { "unavailable" })
    )
    if (!target.isNullOrEmpty()) infoBits.add("target $target")
    infoBits.add(
        "conversation " + (permission.sessionId.ifBlank { "unavailable" })
    )
    val info = wrappedMutedLabel(infoBits.joinToString(" · "))
    if (!target.isNullOrEmpty()) {
        info.accessibleContext.accessibleName = "Action target: $target"
    }
    body.add(info)
    val reasonLabel = WrappedLabel(reason)
    reasonLabel.foreground = textForeground()
    reasonLabel.accessibleContext.accessibleName = "Stated reason: $reason"
    body.add(reasonLabel)
    if (permission.detail.isNotEmpty()) {
        // The EXACT payload (no truncation, no reformatting).
        val payload = compactArea(2, monospace = true)
        payload.text = permission.detail
        payload.accessibleContext.accessibleName = "Request payload"
        body.add(payload)
    }
    if (refusal != null) {
        val refusalLabel = WrappedLabel("Reply refused: $refusal")
        refusalLabel.foreground = semanticForeground(SemanticState.NEGATIVE)
        body.add(refusalLabel)
        body.add(
            wrappedMutedLabel(
                "Refresh re-reads the pending list; the reply is never retried automatically."
            )
        )
    }
    val allow = primaryButton("Allow")
    allow.accessibleContext.accessibleName =
        "Allow approval request ${permission.id}: $headlineText"
    val deny = secondaryButton("Deny")
    deny.accessibleContext.accessibleName =
        "Deny approval request ${permission.id}: $headlineText"
    val details = secondaryButton("Details")
    details.accessibleContext.accessibleName = "Details for approval request ${permission.id}"
    val disclosure = WrappedLabel(
        "Request ${permission.id} · capability ${permission.capability} · " +
            "conversation ${permission.sessionId}"
    )
    disclosure.font = monospacePanelFont()
    disclosure.foreground = mutedForeground()
    disclosure.isVisible = false
    details.addActionListener {
        disclosure.isVisible = !disclosure.isVisible
        details.text = if (disclosure.isVisible) "Hide details" else "Details"
        onSelect()
        body.revalidate()
        body.repaint()
    }
    val claim: (String) -> Unit = { decision ->
        if (!gate.pending(permission.id) && gate.tryBegin(permission.id)) {
            allow.isEnabled = false
            deny.isEnabled = false
            onReply(decision)
        }
    }
    allow.addActionListener { claim("allow") }
    deny.addActionListener { claim("deny") }
    val refresh: JButton? = if (onRefresh == null) {
        null
    } else {
        secondaryButton("Refresh").apply {
            accessibleContext.accessibleName =
                "Refresh the pending approval list for request ${permission.id}"
            addActionListener { onRefresh.invoke() }
        }
    }
    val actions = if (refresh == null) {
        actionRow(allow, deny, details)
    } else {
        actionRow(allow, deny, details, refresh)
    }
    body.add(actions)
    body.add(disclosure)
    val card = ApprovalRequestCard(body, allow, deny, details, refresh)
    card.setPending(gate.pending(permission.id))
    body.accessibleContext.accessibleName =
        "$APPROVAL_HEADLINE request ${permission.id}"
    return card
}

/**
 * The bounded Work attention card: the live pending request set rendered
 * directly above the composer. The card hides itself when nothing is
 * pending; at most [MAX_WORK_APPROVAL_CARDS] requests render inline and the
 * remainder is named (the full list stays on Inspect -> Agents -> Approvals).
 * A newly pending request is announced once through [announcementSink]
 * (default: the card's accessible visible-data channel); nothing here ever
 * requests focus, so typing is never interrupted.
 */
internal class WorkApprovalCard(private val gate: ApprovalReplyGate) : JPanel(BorderLayout()) {

    interface Listener {
        fun onReply(permission: NativePermissionEntry, decision: String)
        fun onRefresh()
    }

    /** Test/embedding seam; null uses the accessible announcement channel. */
    internal var announcementSink: ((String) -> Unit)? = null

    private var listener: Listener? = null

    private var permissions: List<NativePermissionEntry> = emptyList()

    private var appliedGateRevision = -1L

    private val refusals = LinkedHashMap<String, String>()

    private val announced = LinkedHashSet<String>()

    private val announcedLog = ArrayList<String>()

    private val cards = JPanel()

    private val overflow = wrappedMutedLabel("")

    private val rendered = LinkedHashMap<String, ApprovalRequestCard>()

    private val renderedReasons = LinkedHashMap<String, String>()

    private val renderedPayloads = LinkedHashMap<String, String>()

    private val content: JPanel

    init {
        cards.layout = javax.swing.BoxLayout(cards, javax.swing.BoxLayout.Y_AXIS)
        cards.isOpaque = false
        val body = pageColumn(gap = Spacing.XS, padding = 0)
        val supporting = wrappedMutedLabel(APPROVAL_SUPPORTING)
        supporting.accessibleContext.accessibleName = APPROVAL_SUPPORTING
        body.add(supporting)
        body.add(vSpace(Spacing.S))
        body.add(cards)
        body.add(overflow)
        content = card(APPROVAL_HEADLINE, body)
        content.accessibleContext.accessibleName = APPROVAL_HEADLINE
        isOpaque = false
        add(content, BorderLayout.CENTER)
        isVisible = false
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the pending set (same snapshot the Approvals list renders). */
    fun update(permissionList: List<NativePermissionEntry>) {
        // Identical snapshot + unchanged gate = no rebuild (the host posts
        // the same pending set on every slow refresh). A claim/finish bumps
        // the gate revision and must re-render the button state.
        val gateRevision = gate.revision()
        if (permissionList == permissions && gateRevision == appliedGateRevision) return
        appliedGateRevision = gateRevision
        permissions = permissionList
        val live = HashSet<String>()
        for (permission in permissionList) live.add(permission.id)
        val staleRefusals = refusals.keys.filter { !live.contains(it) }
        for (id in staleRefusals) refusals.remove(id)
        announced.retainAll(live)
        val fresh = ArrayList<NativePermissionEntry>()
        for (permission in permissionList) {
            if (announced.add(permission.id)) fresh.add(permission)
        }
        render()
        isVisible = permissionList.isNotEmpty()
        for (permission in fresh) {
            announce("$APPROVAL_HEADLINE: ${approvalHeadlineOf(permission)}")
        }
    }

    /** One typed refusal: keep the request visible, release its buttons. */
    fun setReplyRefusal(permissionId: String, detailText: String) {
        refusals[permissionId] = detailText
        render()
    }

    fun count(): Int = permissions.size

    fun isReplyPendingForTest(permissionId: String): Boolean = gate.pending(permissionId)

    fun replyButtonsForTest(permissionId: String): Pair<JButton, JButton>? {
        val card = rendered[permissionId] ?: return null
        return Pair(card.allowButton, card.denyButton)
    }

    fun refreshButtonForTest(permissionId: String): JButton? =
        rendered[permissionId]?.refreshButton

    fun reasonTextForTest(permissionId: String): String? = renderedReasons[permissionId]

    fun payloadTextForTest(permissionId: String): String? = renderedPayloads[permissionId]

    fun refusalTextForTest(permissionId: String): String? = refusals[permissionId]

    fun announcementsForTest(): List<String> = ArrayList(announcedLog)

    fun headlineText(): String = APPROVAL_HEADLINE

    fun supportingText(): String = APPROVAL_SUPPORTING

    private fun announce(text: String) {
        announcedLog.add(text)
        // The standard Swing channel assistive technology listens to. No
        // focus is requested: an approval never steals the composer.
        try {
            accessibleContext.firePropertyChange(
                javax.accessibility.AccessibleContext.ACCESSIBLE_VISIBLE_DATA_PROPERTY,
                null,
                text
            )
        } catch (ignored: Exception) {
            // The announcement channel is best-effort by contract.
        }
        announcementSink?.invoke(text)
    }

    private fun render() {
        rendered.clear()
        renderedReasons.clear()
        renderedPayloads.clear()
        cards.removeAll()
        val shown = permissions.take(MAX_WORK_APPROVAL_CARDS)
        for (permission in shown) {
            cards.add(vSpace(Spacing.XS))
            val card = approvalRequestCard(
                permission = permission,
                gate = gate,
                onReply = { decision -> listener?.onReply(permission, decision) },
                onRefresh = { listener?.onRefresh() },
                refusal = refusals[permission.id]
            )
            rendered[permission.id] = card
            renderedReasons[permission.id] = approvalReasonText(permission)
            renderedPayloads[permission.id] = permission.detail
            cards.add(card.body)
        }
        val hidden = permissions.size - shown.size
        if (hidden > 0) {
            overflow.fullText =
                "+$hidden more pending — open Inspect → Agents → Approvals for the full list."
            overflow.isVisible = true
        } else {
            overflow.fullText = ""
            overflow.isVisible = false
        }
        cards.revalidate()
        cards.repaint()
        revalidate()
        repaint()
    }
}
